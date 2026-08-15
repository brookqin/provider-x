use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use chrono::{SecondsFormat, Utc};
use gpui::Global;
use provider_x_catalog::{ManualDiscoveryClient, RefreshPreview};
use provider_x_core::{ProviderConfig, ProviderId, ProviderModelCache};
use provider_x_egress::{EgressServer, EgressState, IngressCapability};
use tokio::{
    runtime::Runtime,
    sync::{oneshot, watch},
    task::JoinHandle,
};

use crate::codex_config::{CodexConfigEditor, CodexConfigStatus, CodexIntegration, ReceiptPhase};
use crate::control_plane::{AppPaths, ControlMutation, ControlPlane};
use crate::storage::{
    ModelRegistryStore, ModelRegistryStoreError, SecureFileError, SingleInstanceGuard,
};

const STARTUP_HANDOFF_WAIT: std::time::Duration = std::time::Duration::from_secs(35);
const LISTENER_PORT_RANGE_WIDTH: u16 = 10;

pub(crate) struct ManualRefreshOutcome {
    pub(crate) preview: RefreshPreview,
    pub(crate) registry_matched_models: usize,
    pub(crate) registry_warning: Option<String>,
}

pub(crate) struct ProviderMutationOutcome {
    pub(crate) provider_id: ProviderId,
    pub(crate) codex_warning: Option<String>,
}

#[derive(Clone)]
pub(crate) struct AppServices {
    pub(crate) egress: Arc<EgressHandle>,
    runtime: Arc<Runtime>,
    pub(crate) control: Arc<Mutex<ControlPlane>>,
    pub(crate) model_registry: ModelRegistryStore,
    pub(crate) codex_config: CodexConfigEditor,
    // Rust drops fields in declaration order. Keep the process lock last so a
    // successor cannot start until the egress handle and runtime are gone.
    _single_instance: Arc<SingleInstanceGuard>,
}

pub(crate) struct EgressHandle {
    pub(crate) state: Arc<EgressState>,
    pub(crate) address: SocketAddr,
    ingress_capability: IngressCapability,
    shutdown: watch::Sender<bool>,
    failure: Arc<Mutex<Option<String>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    shutdown_wait: std::time::Duration,
}

impl Drop for EgressHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

impl Global for AppServices {}

impl AppServices {
    pub(crate) fn new() -> anyhow::Result<Self> {
        Self::new_with_listener_port(None)
    }

    pub(crate) fn new_with_listener_port(listener_port: Option<u16>) -> anyhow::Result<Self> {
        let home = PathBuf::from(
            std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?,
        );
        let paths = AppPaths::for_home(home.clone());
        let single_instance = Arc::new(SingleInstanceGuard::acquire_with_timeout(
            paths.root.join("provider-x.lock"),
            STARTUP_HANDOFF_WAIT,
        )?);
        let control = ControlPlane::load(&paths)?;
        let model_registry = ModelRegistryStore::new(&paths.model_registry);
        let codex_config =
            CodexConfigEditor::new(home.join(".codex/config.toml"), &paths.install_receipt);
        let active_integration = codex_config.active_integration()?;
        let ingress_capability = active_integration
            .as_ref()
            .and_then(|integration| capability_from_base_url(&integration.openai_base_url))
            .map_or_else(generate_ingress_capability, Ok)?;
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("provider-x-control")
                .build()?,
        );
        let listener = &control.providers().listener;
        let listener_ip = listener.host.parse::<IpAddr>()?;
        let egress_state = Arc::new(EgressState::new(
            control.providers(),
            control.cache(),
            "https://chatgpt.com/backend-api/codex",
            ingress_capability.clone(),
        )?);
        let server = if let Some(port) = listener_port {
            runtime.block_on(EgressServer::bind(
                SocketAddr::new(listener_ip, port),
                Arc::clone(&egress_state),
            ))?
        } else {
            runtime.block_on(EgressServer::bind_first_available(
                listener_ip,
                listener.port..=listener.port.saturating_add(LISTENER_PORT_RANGE_WIDTH),
                Arc::clone(&egress_state),
            ))?
        };
        let address = server.local_addr()?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let failure = Arc::new(Mutex::new(None));
        let failure_task = Arc::clone(&failure);
        let task = runtime.spawn(async move {
            if let Err(error) = server.run(shutdown_rx).await
                && let Ok(mut failure) = failure_task.lock()
            {
                *failure = Some(error.to_string());
            }
        });
        runtime.block_on(tokio::net::TcpStream::connect(address))?;
        let shutdown_wait = std::time::Duration::from_millis(
            control
                .providers()
                .timeouts
                .shutdown_grace_ms
                .saturating_add(3_000),
        );
        let services = Self {
            egress: Arc::new(EgressHandle {
                state: egress_state,
                address,
                ingress_capability,
                shutdown,
                failure,
                task: Mutex::new(Some(task)),
                shutdown_wait,
            }),
            runtime,
            control: Arc::new(Mutex::new(control)),
            model_registry,
            codex_config,
            _single_instance: single_instance,
        };
        if listener_port.is_none() && active_integration.is_some() {
            services
                .reconcile_codex_listener_if_active()
                .map_err(anyhow::Error::msg)?;
        }
        Ok(services)
    }

    pub(crate) fn spawn<F, T>(&self, future: F) -> oneshot::Receiver<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.runtime.spawn(async move {
            let output = future.await;
            let _ = sender.send(output);
        });
        receiver
    }

    pub(crate) fn spawn_blocking<F, T>(&self, operation: F) -> oneshot::Receiver<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        self.runtime.spawn_blocking(move || {
            let output = operation();
            let _ = sender.send(output);
        });
        receiver
    }

    pub(crate) fn apply_provider_mutation(
        &self,
        mutation: ControlMutation,
    ) -> Result<ProviderMutationOutcome, String> {
        let provider_id = {
            let mut control = self
                .control
                .lock()
                .map_err(|_| rust_i18n::t!("app.internal.control_lock").to_string())?;
            let prepared = control
                .prepare_mutation(mutation)
                .map_err(|error| error.to_string())?;
            let (providers, cache) = prepared.documents();
            let egress_reload = self
                .egress
                .state
                .prepare_reload(providers, cache)
                .map_err(|error| error.to_string())?;
            let provider_id = control
                .commit_mutation(prepared)
                .map_err(|error| error.to_string())?;
            self.egress.state.commit_reload(egress_reload);
            provider_id
        };

        Ok(ProviderMutationOutcome {
            provider_id,
            codex_warning: self.reconcile_codex_if_active().err(),
        })
    }

    pub(crate) async fn refresh_provider_models(
        &self,
        client: ManualDiscoveryClient,
        provider: ProviderConfig,
        existing: Option<ProviderModelCache>,
        timestamp: String,
    ) -> Result<ManualRefreshOutcome, String> {
        let mut preview = client
            .refresh_preview(&provider, existing.as_ref(), timestamp.clone())
            .await
            .map_err(|error| error.to_string())?;
        if preview.needs_review.is_empty() {
            return Ok(ManualRefreshOutcome {
                preview,
                registry_matched_models: 0,
                registry_warning: None,
            });
        }

        let store = self.model_registry.clone();
        let loaded = tokio::task::spawn_blocking(move || match store.load() {
            Ok(loaded) => Ok(Some(loaded)),
            Err(ModelRegistryStoreError::File(SecureFileError::MissingFile(_))) => Ok(None),
            Err(error) => Err(error.to_string()),
        })
        .await
        .map_err(|_| rust_i18n::t!("app.internal.registry_read_task").to_string())?;
        let (cached, expected_sha256, mut warnings) = match loaded {
            Ok(Some(loaded)) => (
                Some(loaded.document),
                Some(loaded.sha256),
                Vec::<String>::new(),
            ),
            Ok(None) => (None, None, Vec::new()),
            Err(warning) => (None, None, vec![warning]),
        };
        let enrichment = client
            .enrich_preview_with_registry(&provider, &mut preview, cached.as_ref(), timestamp)
            .await;
        if let Some(warning) = enrichment.warning {
            warnings.push(warning);
        }
        if let Some(replacement) = enrichment.replacement_cache {
            let store = self.model_registry.clone();
            let save_result = tokio::task::spawn_blocking(move || {
                store
                    .save(&replacement, expected_sha256.as_deref())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|_| rust_i18n::t!("app.internal.registry_write_task").to_string())?;
            if let Err(warning) = save_result {
                warnings.push(warning);
            }
        }
        Ok(ManualRefreshOutcome {
            preview,
            registry_matched_models: enrichment.matched_models.len(),
            registry_warning: (!warnings.is_empty()).then(|| warnings.join("；")),
        })
    }

    pub(crate) fn egress_ready(&self) -> Result<(), String> {
        if let Some(error) = self
            .egress
            .failure
            .lock()
            .map_err(|_| rust_i18n::t!("app.internal.egress_lock").to_string())?
            .clone()
        {
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn verify_egress_ready(&self) -> Result<(), String> {
        self.egress_ready()?;
        tokio::net::TcpStream::connect(self.egress.address)
            .await
            .map(|_| ())
            .map_err(|error| {
                rust_i18n::t!("app.internal.egress_health_failed", error = error).to_string()
            })
    }

    pub(crate) fn codex_status(&self) -> Result<CodexConfigStatus, String> {
        self.codex_config
            .inspect()
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn set_codex_integration(
        &self,
        enabled: bool,
    ) -> Result<CodexConfigStatus, String> {
        if enabled {
            self.verify_egress_ready().await?;
        }
        let services = self.clone();
        tokio::task::spawn_blocking(move || {
            if enabled {
                services.apply_codex_integration()?;
            } else {
                services
                    .codex_config
                    .restore(timestamp())
                    .map_err(|error| error.to_string())?;
            }
            services.codex_status()
        })
        .await
        .map_err(|_| rust_i18n::t!("app.codex.task_failed").to_string())?
    }

    pub(crate) fn reconcile_codex_if_active(&self) -> Result<(), String> {
        let status = self.codex_status()?;
        if matches!(status.receipt_phase, Some(ReceiptPhase::Active { .. }))
            && status.managed_values_match
        {
            self.apply_codex_integration()?;
        }
        Ok(())
    }

    fn reconcile_codex_listener_if_active(&self) -> Result<(), String> {
        let Some(current) = self
            .codex_config
            .active_integration()
            .map_err(|error| error.to_string())?
        else {
            return Ok(());
        };
        if current != self.desired_codex_integration()? {
            self.apply_codex_integration()?;
        }
        Ok(())
    }

    fn apply_codex_integration(&self) -> Result<(), String> {
        let desired = self.desired_codex_integration()?;
        self.codex_config
            .apply(&desired, timestamp())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn desired_codex_integration(&self) -> Result<CodexIntegration, String> {
        let control = self
            .control
            .lock()
            .map_err(|_| rust_i18n::t!("app.internal.control_lock").to_string())?;
        if !control.providers().codex.manage_user_config {
            return Err(rust_i18n::t!("app.internal.codex_management_disabled").to_string());
        }
        Ok(CodexIntegration {
            openai_base_url: format!(
                "http://{}/{}/v1",
                self.egress.address,
                self.egress.ingress_capability.expose()
            ),
        })
    }

    pub(crate) async fn shutdown_egress(&self) -> Result<(), String> {
        let _ = self.egress.shutdown.send(true);
        let task = self
            .egress
            .task
            .lock()
            .map_err(|_| rust_i18n::t!("app.internal.egress_task_lock").to_string())?
            .take();
        if let Some(mut task) = task {
            if tokio::time::timeout(self.egress.shutdown_wait, &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
                return Err(rust_i18n::t!("app.internal.egress_shutdown_timeout").to_string());
            }
        }
        self.egress_ready()
    }
}

fn generate_ingress_capability() -> anyhow::Result<IngressCapability> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("failed to generate ingress capability: {error}"))?;
    Ok(IngressCapability::from_hex(hex::encode(bytes))?)
}

fn capability_from_base_url(base_url: &str) -> Option<IngressCapability> {
    let (_, authority_and_path) = base_url.split_once("://")?;
    let (_, path) = authority_and_path.split_once('/')?;
    let mut segments = path.split('/');
    let capability = segments.next()?;
    if segments.next() != Some("v1") || segments.next().is_some() {
        return None;
    }
    IngressCapability::from_hex(capability).ok()
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::{capability_from_base_url, generate_ingress_capability};

    #[test]
    fn capability_is_recovered_only_from_the_managed_base_url_shape() {
        let value = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let parsed = capability_from_base_url(&format!("http://127.0.0.1:43120/{value}/v1"))
            .expect("valid managed URL must retain its capability");
        assert_eq!(parsed.expose(), value);
        assert!(capability_from_base_url("http://127.0.0.1:43120/v1").is_none());
        assert!(
            capability_from_base_url(&format!("http://127.0.0.1:43120/{value}/v1/extra")).is_none()
        );
    }

    #[test]
    fn generated_capabilities_are_256_bit_and_debug_redacted() {
        let first = generate_ingress_capability().unwrap();
        let second = generate_ingress_capability().unwrap();
        assert_eq!(first.expose().len(), 64);
        assert_ne!(first, second);
        assert!(!format!("{first:?}").contains(first.expose()));
    }
}
