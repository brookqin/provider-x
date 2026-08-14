use std::{collections::BTreeMap, path::PathBuf};

use provider_x_catalog::{CatalogError, CatalogOverlay, RefreshPreview};
use provider_x_core::{
    CodexConfig, ListenerConfig, ModelCacheDocument, ProviderConfig, ProviderId, ProvidersDocument,
    RuntimeSnapshot, TimeoutConfig,
};
use thiserror::Error;

use crate::storage::{
    ModelCacheStore, ModelCacheStoreError, ProviderConfigStore, ProviderConfigStoreError,
    SecureFileError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub root: PathBuf,
    pub providers: PathBuf,
    pub model_cache: PathBuf,
    pub model_registry: PathBuf,
    pub install_receipt: PathBuf,
    pub ui_locale: PathBuf,
}

impl AppPaths {
    #[must_use]
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        let root = home
            .into()
            .join("Library/Application Support/dev.qiankun.provider-x");
        Self {
            providers: root.join("providers.yaml"),
            model_cache: root.join("cache/models.yaml"),
            model_registry: root.join("cache/model-registry.json"),
            install_receipt: root.join("install-receipt.json"),
            ui_locale: root.join("ui-locale"),
            root,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshCommitOutcome {
    pub provider_id: ProviderId,
}

pub(crate) enum ControlMutation {
    CommitRefresh {
        provider: ProviderConfig,
        preview: RefreshPreview,
    },
    SaveProvider(ProviderConfig),
    SetProviderEnabled {
        provider_id: ProviderId,
        enabled: bool,
    },
    RemoveProvider(ProviderId),
}

pub(crate) struct PreparedControlMutation {
    providers: ProvidersDocument,
    cache: ModelCacheDocument,
    provider_id: ProviderId,
    cache_changed: bool,
}

impl PreparedControlMutation {
    pub(crate) fn documents(&self) -> (&ProvidersDocument, &ModelCacheDocument) {
        (&self.providers, &self.cache)
    }
}

#[derive(Debug, Error)]
pub enum ControlPlaneError {
    #[error(transparent)]
    SecureFile(#[from] SecureFileError),

    #[error(transparent)]
    ProviderStore(#[from] ProviderConfigStoreError),

    #[error(transparent)]
    CacheStore(#[from] ModelCacheStoreError),

    #[error(transparent)]
    Catalog(#[from] CatalogError),

    #[error(transparent)]
    Core(#[from] provider_x_core::CoreError),

    #[error("Provider {0} does not exist")]
    ProviderNotFound(String),

    #[error("refresh preview fingerprint does not match Provider {0}")]
    RefreshFingerprintMismatch(String),

    #[error("Provider save failed after the refreshed cache was written; reload and retry: {0}")]
    ProviderSaveAfterCache(String),
}

pub struct ControlPlane {
    provider_store: ProviderConfigStore,
    cache_store: ModelCacheStore,
    providers: ProvidersDocument,
    provider_sha256: Option<String>,
    cache: ModelCacheDocument,
    cache_sha256: Option<String>,
}

impl ControlPlane {
    /// Loads the last validated documents or creates empty in-memory defaults for a first launch.
    ///
    /// No file is created until an explicit save, refresh commit, or enable-state change.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe files, malformed documents, or an invalid runtime snapshot.
    pub fn load(paths: &AppPaths) -> Result<Self, ControlPlaneError> {
        let provider_store = ProviderConfigStore::new(&paths.providers);
        let cache_store = ModelCacheStore::new(&paths.model_cache);

        let (providers, provider_sha256) = match provider_store.load() {
            Ok(loaded) => (loaded.document, Some(loaded.sha256)),
            Err(ProviderConfigStoreError::File(SecureFileError::MissingFile(_))) => {
                (default_providers(), None)
            }
            Err(error) => return Err(error.into()),
        };
        let (cache, cache_sha256) = match cache_store.load() {
            Ok(loaded) => (loaded.document, Some(loaded.sha256)),
            Err(ModelCacheStoreError::File(SecureFileError::MissingFile(_))) => {
                (empty_cache(), None)
            }
            Err(error) => return Err(error.into()),
        };
        RuntimeSnapshot::build(&providers, &cache)?;
        CatalogOverlay::from_documents(&providers, &cache)?;

        Ok(Self {
            provider_store,
            cache_store,
            providers,
            provider_sha256,
            cache,
            cache_sha256,
        })
    }

    #[must_use]
    pub fn providers(&self) -> &ProvidersDocument {
        &self.providers
    }

    #[must_use]
    pub fn cache(&self) -> &ModelCacheDocument {
        &self.cache
    }

    /// Commits one successful user-confirmed refresh without changing the Provider's enable state.
    /// Cache is written first; a crash before the matching Provider write leaves a fingerprint
    /// mismatch that fails closed instead of changing the persisted enable flag.
    ///
    /// # Errors
    ///
    /// Returns an error for fingerprint mismatch, validation, concurrent changes, or file writes.
    /// A Provider-write failure after cache publication must leave the prepared Egress runtime
    /// unpublished; callers should reload the editor state and retry.
    pub fn commit_refresh(
        &mut self,
        provider: ProviderConfig,
        preview: RefreshPreview,
    ) -> Result<RefreshCommitOutcome, ControlPlaneError> {
        let prepared =
            self.prepare_mutation(ControlMutation::CommitRefresh { provider, preview })?;
        Ok(RefreshCommitOutcome {
            provider_id: self.commit_mutation(prepared)?,
        })
    }

    /// Saves one Provider configuration without refreshing or changing its model cache.
    ///
    /// # Errors
    ///
    /// Returns an error for validation, stale enabled routes, or concurrent writes.
    pub fn save_provider(
        &mut self,
        provider: ProviderConfig,
    ) -> Result<ProviderId, ControlPlaneError> {
        let prepared = self.prepare_mutation(ControlMutation::SaveProvider(provider))?;
        self.commit_mutation(prepared)
    }

    /// Enables or disables one saved Provider after validating its route and catalog overlay.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown Providers, stale cache, candidate rejection, concurrent file
    /// changes, or an invalid private catalog projection.
    pub fn set_provider_enabled(
        &mut self,
        provider_id: &ProviderId,
        enabled: bool,
    ) -> Result<(), ControlPlaneError> {
        let prepared = self.prepare_mutation(ControlMutation::SetProviderEnabled {
            provider_id: provider_id.clone(),
            enabled,
        })?;
        self.commit_mutation(prepared)?;
        Ok(())
    }

    /// Removes one Provider from configuration and runtime projection while retaining its cached
    /// model metadata. Re-adding the same Provider ID can reuse that cache when its fingerprint
    /// still matches.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown Provider, candidate validation, or concurrent file changes.
    pub fn remove_provider(&mut self, provider_id: &ProviderId) -> Result<(), ControlPlaneError> {
        let prepared =
            self.prepare_mutation(ControlMutation::RemoveProvider(provider_id.clone()))?;
        self.commit_mutation(prepared)?;
        Ok(())
    }

    pub(crate) fn prepare_mutation(
        &self,
        mutation: ControlMutation,
    ) -> Result<PreparedControlMutation, ControlPlaneError> {
        let (providers, cache, provider_id, cache_changed) = match mutation {
            ControlMutation::CommitRefresh { provider, preview } => {
                if preview.cache.config_fingerprint != provider.routing_fingerprint()? {
                    return Err(ControlPlaneError::RefreshFingerprintMismatch(
                        provider.id.to_string(),
                    ));
                }
                let provider_id = provider.id.clone();
                let mut providers = self.providers.clone();
                upsert_provider(&mut providers, provider);
                let mut cache = self.cache.clone();
                cache.providers.insert(provider_id.clone(), preview.cache);
                (providers, cache, provider_id, true)
            }
            ControlMutation::SaveProvider(provider) => {
                let provider_id = provider.id.clone();
                let mut providers = self.providers.clone();
                upsert_provider(&mut providers, provider);
                (providers, self.cache.clone(), provider_id, false)
            }
            ControlMutation::SetProviderEnabled {
                provider_id,
                enabled,
            } => {
                let mut providers = self.providers.clone();
                let provider = providers
                    .providers
                    .iter_mut()
                    .find(|provider| provider.id == provider_id)
                    .ok_or_else(|| ControlPlaneError::ProviderNotFound(provider_id.to_string()))?;
                provider.enabled = enabled;
                (providers, self.cache.clone(), provider_id, false)
            }
            ControlMutation::RemoveProvider(provider_id) => {
                if !self
                    .providers
                    .providers
                    .iter()
                    .any(|provider| provider.id == provider_id)
                {
                    return Err(ControlPlaneError::ProviderNotFound(provider_id.to_string()));
                }
                let mut providers = self.providers.clone();
                providers
                    .providers
                    .retain(|provider| provider.id != provider_id);
                (providers, self.cache.clone(), provider_id, false)
            }
        };

        RuntimeSnapshot::build(&providers, &cache)?;
        CatalogOverlay::from_documents(&providers, &cache)?;
        Ok(PreparedControlMutation {
            providers,
            cache,
            provider_id,
            cache_changed,
        })
    }

    pub(crate) fn commit_mutation(
        &mut self,
        prepared: PreparedControlMutation,
    ) -> Result<ProviderId, ControlPlaneError> {
        crate::storage::atomic_file::ensure_private_directory(&self.paths_root())?;
        let saved_cache = if prepared.cache_changed {
            Some(
                self.cache_store
                    .save(&prepared.cache, self.cache_sha256.as_deref())?,
            )
        } else {
            None
        };
        let saved_provider = match self
            .provider_store
            .save(&prepared.providers, self.provider_sha256.as_deref())
        {
            Ok(saved) => saved,
            Err(error) => {
                if let Some(saved_cache) = saved_cache {
                    self.cache = prepared.cache;
                    self.cache_sha256 = Some(saved_cache.sha256);
                    return Err(ControlPlaneError::ProviderSaveAfterCache(error.to_string()));
                }
                return Err(error.into());
            }
        };

        self.providers = prepared.providers;
        self.provider_sha256 = Some(saved_provider.sha256);
        self.cache = prepared.cache;
        if let Some(saved_cache) = saved_cache {
            self.cache_sha256 = Some(saved_cache.sha256);
        }
        Ok(prepared.provider_id)
    }

    fn paths_root(&self) -> PathBuf {
        self.provider_store
            .path()
            .parent()
            .expect("Provider store always has a parent")
            .to_path_buf()
    }
}

fn upsert_provider(document: &mut ProvidersDocument, provider: ProviderConfig) {
    if let Some(existing) = document
        .providers
        .iter_mut()
        .find(|existing| existing.id == provider.id)
    {
        *existing = provider;
    } else {
        document.providers.push(provider);
        document
            .providers
            .sort_by(|left, right| left.id.cmp(&right.id));
    }
}

fn default_providers() -> ProvidersDocument {
    ProvidersDocument {
        schema_version: 1,
        listener: ListenerConfig {
            host: "127.0.0.1".to_owned(),
            port: 43_119,
            request_body_limit_bytes: 32 * 1024 * 1024,
            max_connections: 128,
        },
        timeouts: TimeoutConfig {
            request_body_ms: 30_000,
            connect_ms: 10_000,
            response_headers_ms: 30_000,
            stream_idle_ms: 300_000,
            websocket_idle_ms: 300_000,
            shutdown_grace_ms: 30_000,
        },
        codex: CodexConfig {
            manage_user_config: true,
        },
        providers: Vec::new(),
    }
}

fn empty_cache() -> ModelCacheDocument {
    ModelCacheDocument {
        schema_version: 1,
        providers: BTreeMap::new(),
    }
}
