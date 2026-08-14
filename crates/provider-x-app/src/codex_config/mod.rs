mod receipt;

use std::{fmt, path::PathBuf, str::FromStr};

use sha2::{Digest, Sha256};
use thiserror::Error;
use toml_edit::{DocumentMut, Item, value};

use crate::storage::{SecureFileError, atomic_file};

pub use receipt::{
    AppliedCodexValues, INSTALL_RECEIPT_SCHEMA_VERSION, InstallReceipt, InstallReceiptStore,
    LoadedInstallReceipt, OriginalCodexConfig, OriginalCodexModelCache, ReceiptPhase,
    ReceiptStoreError,
};

const OPENAI_BASE_URL: &str = "openai_base_url";
const MODEL_CATALOG_JSON: &str = "model_catalog_json";
const MODEL_PROVIDER: &str = "model_provider";

#[derive(Clone, PartialEq, Eq)]
pub struct CodexIntegration {
    pub openai_base_url: String,
}

impl fmt::Debug for CodexIntegration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexIntegration")
            .field("openai_base_url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexConfigStatus {
    pub config_exists: bool,
    pub receipt_phase: Option<ReceiptPhase>,
    pub managed_values_match: bool,
}

#[derive(Debug, Error)]
pub enum CodexConfigError {
    #[error(transparent)]
    File(#[from] SecureFileError),

    #[error(transparent)]
    Receipt(#[from] ReceiptStoreError),

    #[error("invalid Codex config TOML: {0}")]
    InvalidToml(String),

    #[error("invalid Codex model cache: {0}")]
    InvalidModelCache(String),

    #[error("install receipt belongs to another Codex config path")]
    ReceiptPathMismatch,

    #[error("Codex config changed after provider-x last inspected it; reload before continuing")]
    ConfigDrift,
}

#[derive(Clone, Debug)]
pub struct CodexConfigEditor {
    config_path: PathBuf,
    model_cache_path: PathBuf,
    receipt_store: InstallReceiptStore,
}

impl CodexConfigEditor {
    #[must_use]
    pub fn new(config_path: impl Into<PathBuf>, receipt_path: impl Into<PathBuf>) -> Self {
        let config_path = config_path.into();
        let model_cache_path = config_path.parent().map_or_else(
            || PathBuf::from("models_cache.json"),
            |parent| parent.join("models_cache.json"),
        );
        Self {
            config_path,
            model_cache_path,
            receipt_store: InstallReceiptStore::new(receipt_path),
        }
    }

    /// Reads the current configuration and receipt without mutating either file.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe files, malformed TOML/receipt data, or a receipt for another
    /// Codex home.
    pub fn inspect(&self) -> Result<CodexConfigStatus, CodexConfigError> {
        let config = self.load_config()?;
        let receipt = self.load_receipt()?;
        if let Some(loaded) = &receipt {
            self.validate_receipt_path(&loaded.receipt)?;
        }
        let managed_values_match = receipt
            .as_ref()
            .is_some_and(|loaded| managed_state_matches(&config.document, &loaded.receipt));
        Ok(CodexConfigStatus {
            config_exists: config.sha256.is_some(),
            receipt_phase: receipt.map(|loaded| loaded.receipt.phase),
            managed_values_match,
        })
    }

    /// Returns the currently active, unchanged managed integration.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe files, malformed TOML/receipt data, or a receipt belonging to
    /// another Codex home.
    pub fn active_integration(&self) -> Result<Option<CodexIntegration>, CodexConfigError> {
        let config = self.load_config()?;
        let Some(loaded) = self.load_receipt()? else {
            return Ok(None);
        };
        self.validate_receipt_path(&loaded.receipt)?;
        if !matches!(loaded.receipt.phase, ReceiptPhase::Active { .. })
            || !managed_state_matches(&config.document, &loaded.receipt)
        {
            return Ok(None);
        }
        Ok(Some(CodexIntegration {
            openai_base_url: loaded.receipt.applied_values.openai_base_url,
        }))
    }

    /// Atomically applies the Codex integration and persists a crash-recoverable
    /// receipt before changing the user configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for configuration conflicts, unsafe files, concurrent changes, malformed
    /// TOML/receipt data, or failed atomic writes.
    pub fn apply(
        &self,
        desired: &CodexIntegration,
        updated_at: impl Into<String>,
    ) -> Result<InstallReceipt, CodexConfigError> {
        let updated_at = updated_at.into();
        let config = self.load_config()?;
        let current_model_cache = self.load_model_cache()?;
        let loaded_receipt = self.load_receipt()?;
        let (original, original_model_cache, receipt_sha256) = if let Some(loaded) = loaded_receipt
        {
            self.validate_receipt_path(&loaded.receipt)?;
            if loaded.receipt.phase == ReceiptPhase::Restored {
                (
                    OriginalCodexConfig::from_current(&config),
                    OriginalCodexModelCache::from_current(current_model_cache.as_ref())?,
                    Some(loaded.sha256),
                )
            } else {
                Self::validate_current_against_receipt(&config, &loaded.receipt)?;
                (
                    loaded.receipt.original,
                    loaded.receipt.original_model_cache.unwrap_or(
                        OriginalCodexModelCache::from_current(current_model_cache.as_ref())?,
                    ),
                    Some(loaded.sha256),
                )
            }
        } else {
            (
                OriginalCodexConfig::from_current(&config),
                OriginalCodexModelCache::from_current(current_model_cache.as_ref())?,
                None,
            )
        };

        let (candidate_bytes, applied_values) = prepare_candidate(&config.document, desired);
        let candidate_sha256 = sha256(&candidate_bytes);
        let prepared = InstallReceipt {
            schema_version: INSTALL_RECEIPT_SCHEMA_VERSION,
            config_path: self.config_path.to_string_lossy().into_owned(),
            updated_at: updated_at.clone(),
            phase: ReceiptPhase::Prepared {
                planned_sha256: candidate_sha256.clone(),
            },
            original,
            original_model_cache: Some(original_model_cache),
            applied_values: applied_values.clone(),
        };
        let saved_prepared = self
            .receipt_store
            .save(&prepared, receipt_sha256.as_deref())?;
        atomic_file::write_external(
            &self.config_path,
            config.sha256.as_deref(),
            &candidate_bytes,
        )?;
        self.invalidate_model_cache(current_model_cache.as_ref())?;
        let active = InstallReceipt {
            phase: ReceiptPhase::Active {
                applied_sha256: candidate_sha256,
            },
            updated_at,
            applied_values,
            ..prepared
        };
        self.receipt_store
            .save(&active, Some(&saved_prepared.sha256))?;
        Ok(active)
    }

    /// Restores the exact pre-install Codex config after verifying that no external edits occurred.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing/invalid receipt, unsafe files, or any config drift.
    pub fn restore(
        &self,
        updated_at: impl Into<String>,
    ) -> Result<InstallReceipt, CodexConfigError> {
        let loaded = self.receipt_store.load()?;
        self.validate_receipt_path(&loaded.receipt)?;
        if loaded.receipt.phase == ReceiptPhase::Restored {
            return Ok(loaded.receipt);
        }
        let config = self.load_config()?;
        Self::validate_current_against_receipt(&config, &loaded.receipt)?;
        let current_model_cache = self.load_model_cache()?;
        self.invalidate_model_cache(current_model_cache.as_ref())?;
        let original_document = parse_document(&loaded.receipt.original.contents)?;
        let mut candidate = config.document.clone();
        for key in [OPENAI_BASE_URL, MODEL_CATALOG_JSON, MODEL_PROVIDER] {
            restore_item(&mut candidate, &original_document, key);
        }
        let candidate_contents = candidate.to_string();
        if !loaded.receipt.original.existed && candidate_contents.trim().is_empty() {
            if let Some(current_sha) = config.sha256.as_deref() {
                atomic_file::remove_external(&self.config_path, current_sha)?;
            }
        } else if candidate_contents != config.contents {
            atomic_file::write_external(
                &self.config_path,
                config.sha256.as_deref(),
                candidate_contents.as_bytes(),
            )?;
        }
        let restored = InstallReceipt {
            phase: ReceiptPhase::Restored,
            updated_at: updated_at.into(),
            ..loaded.receipt
        };
        self.receipt_store.save(&restored, Some(&loaded.sha256))?;
        Ok(restored)
    }

    fn load_config(&self) -> Result<CurrentCodexConfig, CodexConfigError> {
        match atomic_file::load(&self.config_path) {
            Ok(loaded) => {
                let contents = String::from_utf8(loaded.bytes)
                    .map_err(|error| CodexConfigError::InvalidToml(error.to_string()))?;
                Ok(CurrentCodexConfig {
                    document: parse_document(&contents)?,
                    contents,
                    sha256: Some(loaded.sha256),
                })
            }
            Err(SecureFileError::MissingFile(_)) => Ok(CurrentCodexConfig {
                document: DocumentMut::new(),
                contents: String::new(),
                sha256: None,
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn load_receipt(&self) -> Result<Option<LoadedInstallReceipt>, CodexConfigError> {
        match self.receipt_store.load() {
            Ok(loaded) => Ok(Some(loaded)),
            Err(ReceiptStoreError::File(SecureFileError::MissingFile(_))) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn load_model_cache(&self) -> Result<Option<crate::storage::LoadedFile>, CodexConfigError> {
        match atomic_file::load_external_cache(&self.model_cache_path) {
            Ok(loaded) => Ok(Some(loaded)),
            Err(SecureFileError::MissingFile(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn invalidate_model_cache(
        &self,
        current: Option<&crate::storage::LoadedFile>,
    ) -> Result<(), CodexConfigError> {
        if let Some(current) = current {
            atomic_file::remove_external_cache(&self.model_cache_path, &current.sha256)?;
        }
        Ok(())
    }

    fn validate_receipt_path(&self, receipt: &InstallReceipt) -> Result<(), CodexConfigError> {
        if receipt.config_path == self.config_path.to_string_lossy() {
            Ok(())
        } else {
            Err(CodexConfigError::ReceiptPathMismatch)
        }
    }

    fn validate_current_against_receipt(
        config: &CurrentCodexConfig,
        receipt: &InstallReceipt,
    ) -> Result<(), CodexConfigError> {
        let matches = match &receipt.phase {
            ReceiptPhase::Active { .. } => managed_state_matches(&config.document, receipt),
            ReceiptPhase::Prepared { planned_sha256 } => {
                config.sha256.as_deref() == Some(planned_sha256)
                    || managed_state_matches(&config.document, receipt)
                    || original_managed_state_matches(&config.document, receipt)
            }
            ReceiptPhase::Restored => original_managed_state_matches(&config.document, receipt),
        };
        if matches {
            Ok(())
        } else {
            Err(CodexConfigError::ConfigDrift)
        }
    }
}

fn prepare_candidate(
    current: &DocumentMut,
    desired: &CodexIntegration,
) -> (Vec<u8>, AppliedCodexValues) {
    let mut candidate = current.clone();
    candidate[OPENAI_BASE_URL] = value(&desired.openai_base_url);
    candidate.remove(MODEL_CATALOG_JSON);
    candidate[MODEL_PROVIDER] = value("openai");
    let applied_values = AppliedCodexValues {
        openai_base_url: desired.openai_base_url.clone(),
    };
    (candidate.to_string().into_bytes(), applied_values)
}

struct CurrentCodexConfig {
    document: DocumentMut,
    contents: String,
    sha256: Option<String>,
}

impl OriginalCodexConfig {
    fn from_current(current: &CurrentCodexConfig) -> Self {
        Self {
            existed: current.sha256.is_some(),
            sha256: current.sha256.clone(),
            contents: current.contents.clone(),
        }
    }
}

impl OriginalCodexModelCache {
    fn from_current(
        current: Option<&crate::storage::LoadedFile>,
    ) -> Result<Self, CodexConfigError> {
        let Some(current) = current else {
            return Ok(Self {
                existed: false,
                sha256: None,
                contents: String::new(),
            });
        };
        let contents = String::from_utf8(current.bytes.clone())
            .map_err(|error| CodexConfigError::InvalidModelCache(error.to_string()))?;
        Ok(Self {
            existed: true,
            sha256: Some(current.sha256.clone()),
            contents,
        })
    }
}

fn parse_document(contents: &str) -> Result<DocumentMut, CodexConfigError> {
    DocumentMut::from_str(contents)
        .map_err(|error| CodexConfigError::InvalidToml(error.to_string()))
}

fn restore_item(candidate: &mut DocumentMut, original: &DocumentMut, key: &str) {
    if let Some(item) = original.get(key) {
        candidate.insert(key, item.clone());
    } else {
        candidate.remove(key);
    }
}

fn managed_state_matches(document: &DocumentMut, receipt: &InstallReceipt) -> bool {
    document.get(OPENAI_BASE_URL).and_then(Item::as_str)
        == Some(receipt.applied_values.openai_base_url.as_str())
        && document.get(MODEL_CATALOG_JSON).is_none()
        && document.get(MODEL_PROVIDER).and_then(Item::as_str) == Some("openai")
}

fn original_managed_state_matches(document: &DocumentMut, receipt: &InstallReceipt) -> bool {
    let Ok(original) = parse_document(&receipt.original.contents) else {
        return false;
    };
    [OPENAI_BASE_URL, MODEL_CATALOG_JSON, MODEL_PROVIDER]
        .into_iter()
        .all(|key| items_match(document.get(key), original.get(key)))
}

fn items_match(left: Option<&Item>, right: Option<&Item>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => match (left.as_str(), right.as_str()) {
            (Some(left), Some(right)) => left == right,
            _ => left.to_string().trim() == right.to_string().trim(),
        },
        _ => false,
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
