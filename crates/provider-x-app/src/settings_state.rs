use std::sync::{Arc, Mutex};

use gpui_kit::{App, Global};
use provider_x_core::{
    AuthConfig, CatalogModelId, EndpointConfig, ModelId, ProtocolId, ProviderConfig, ProviderId,
    ProviderModelSpec, TransportConfig,
};

use crate::runtime::AppServices;

#[derive(Clone, Debug, Default)]
pub(crate) struct ModelDraft {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub context_window: Option<u64>,
    pub reasoning_levels: Vec<String>,
    pub parallel_tools: Option<bool>,
    pub search_tool: Option<bool>,
    pub metadata_sources: std::collections::BTreeMap<String, provider_x_core::MetadataSource>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProviderDraft {
    pub id: String,
    pub preset: String,
    pub connection: String,
    pub name: String,
    pub endpoint: String,
    pub model_endpoint: String,
    pub websocket_endpoint: String,
    pub websocket: bool,
    pub protocol: ProtocolId,
    pub models: Vec<ModelDraft>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum OperationKind {
    #[default]
    General,
    Login,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SettingKind {
    Theme,
    Locale,
    Dock,
    Startup,
    ProviderEnabled(ProviderId),
    Integration,
    IntegrationStatus,
}

impl SettingKind {
    fn for_action(action: &str) -> Option<Self> {
        match action {
            "theme-system" | "theme-dark" | "theme-light" => Some(Self::Theme),
            "locale-system" | "locale-en" | "locale-zh-CN" => Some(Self::Locale),
            "dock-show" | "dock-hide" => Some(Self::Dock),
            "startup-enable" | "startup-disable" => Some(Self::Startup),
            "integration-enable" | "integration-disable" => Some(Self::Integration),
            "integration-status" => Some(Self::IntegrationStatus),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Session {
    draft: ProviderDraft,
    credential: Option<AuthConfig>,
    message: String,
    operation_kind: OperationKind,
    busy: bool,
    failed: bool,
    revision: u64,
    theme: crate::ui_preferences::ThemePreference,
    locale: crate::localization::UiLocale,
    integration: Option<&'static str>,
    operation: u64,
    cancel: Option<tokio::task::AbortHandle>,
    setting_error: Option<SettingKind>,
    settings_running: bool,
    setting_queue: std::collections::BTreeMap<SettingKind, String>,
}

impl Session {
    fn enqueue_setting(&mut self, kind: SettingKind, action: &str) -> bool {
        // Only the latest queued choice per setting matters; keep the queue bounded.
        self.setting_queue.insert(kind, action.to_owned());
        let start = !self.settings_running;
        self.settings_running = true;
        self.message.clear();
        self.setting_error = None;
        self.failed = false;
        self.revision += 1;
        start
    }

    fn next_setting(&mut self) -> Option<(SettingKind, String)> {
        let next = self.setting_queue.pop_first();
        if next.is_none() && self.settings_running {
            self.settings_running = false;
            self.revision += 1;
        }
        next
    }

    fn finish_setting(&mut self, kind: SettingKind, result: Result<String, String>) {
        match result {
            Err(error) => {
                self.message = error;
                self.failed = true;
                self.operation_kind = OperationKind::General;
                self.setting_error = Some(kind);
            }
            Ok(notice) if matches!(kind, SettingKind::ProviderEnabled(_)) && !notice.is_empty() => {
                self.message = notice;
                self.failed = false;
                self.operation_kind = OperationKind::General;
                self.setting_error = None;
            }
            Ok(_) if self.setting_error.as_ref() == Some(&kind) => {
                self.message.clear();
                self.failed = false;
                self.setting_error = None;
            }
            Ok(_) => {}
        }
        self.revision += 1;
    }
}

#[derive(Clone, Default)]
pub(crate) struct SettingsState(Arc<Mutex<Session>>);
impl Global for SettingsState {}

impl SettingsState {
    pub(crate) fn clear_draft(&self) {
        if let Ok(mut session) = self.0.lock()
            && !session.busy
        {
            session.draft = ProviderDraft::default();
            session.credential = None;
            session.revision += 1;
        }
    }

    pub(crate) fn dismiss_message(&self) {
        if let Ok(mut session) = self.0.lock() {
            session.message.clear();
            session.setting_error = None;
            session.failed = false;
            session.revision += 1;
        }
    }

    pub(crate) fn set_message(&self, message: String) {
        if let Ok(mut session) = self.0.lock() {
            session.operation_kind = OperationKind::General;
            session.setting_error = None;
            session.message = message;
            session.revision += 1;
        }
    }
}

fn host_error() -> anyhow::Error {
    anyhow::anyhow!(rust_i18n::t!("app.internal.control_lock").to_string())
}

pub(crate) fn register(cx: &mut App) -> anyhow::Result<()> {
    if !cx.has_global::<SettingsState>() {
        let host = SettingsState::default();
        let path = crate::control_plane::AppPaths::for_home(crate::runtime::data_home()?)
            .root
            .join("ui-theme.json");
        host.0
            .lock()
            .map_err(|_| anyhow::anyhow!("state unavailable"))?
            .theme = crate::ui_preferences::load(&path)?;
        let paths = crate::control_plane::AppPaths::for_home(crate::runtime::data_home()?);
        host.0
            .lock()
            .map_err(|_| anyhow::anyhow!("state unavailable"))?
            .locale = crate::localization::UiLocaleStore::new(paths.ui_locale)
            .load()?
            .unwrap_or_default();
        cx.set_global(host);
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub auth_label: &'static str,
    pub connection_label: &'static str,
    pub model_count: usize,
    pub enabled_model_count: usize,
    pub model_preview: String,
}

impl From<&ProviderConfig> for ProviderSummary {
    fn from(provider: &ProviderConfig) -> Self {
        let auth_label = match provider.auth {
            AuthConfig::Bearer { .. } => "api",
            AuthConfig::OpenAiOAuth { .. } | AuthConfig::ClaudeCode => "subscription",
            AuthConfig::None => "local",
        };
        let connection_label = provider_x_providers::presets()
            .iter()
            .find(|preset| preset.id == provider.preset)
            .and_then(|preset| {
                preset
                    .connections
                    .iter()
                    .find(|c| c.id == provider.connection)
            })
            .map_or(auth_label, |connection| connection.label);
        let enabled_model_count = provider.models.iter().filter(|model| model.enabled).count();
        let mut model_preview = provider
            .models
            .iter()
            .filter(|model| model.enabled)
            .take(2)
            .map(|model| model.display_name.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        if enabled_model_count > 2 {
            model_preview.push_str(" …");
        }
        Self {
            id: provider.id.to_string(),
            name: provider.name.clone(),
            enabled: provider.enabled,
            auth_label,
            connection_label,
            model_count: provider.models.len(),
            enabled_model_count,
            model_preview,
        }
    }
}

#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)] // Independent UI status flags, not a state machine.
pub(crate) struct SettingsSnapshot {
    pub providers: Vec<ProviderSummary>,
    pub draft: ProviderDraft,
    pub has_credential: bool,
    pub message: String,
    pub operation_kind: OperationKind,
    pub busy: bool,
    pub settings_pending: bool,
    pub failed: bool,
    pub integration: &'static str,
    pub theme: crate::ui_preferences::ThemePreference,
    pub locale: crate::localization::UiLocale,
    pub effective_locale: String,
    pub dock_visible: bool,
    pub startup: crate::platform::macos::LaunchAtLoginStatus,
    pub cancellable: bool,
    pub revision: u64,
}

impl SettingsState {
    pub(crate) fn snapshot(&self, services: &AppServices) -> anyhow::Result<SettingsSnapshot> {
        let session = self.0.lock().map_err(|_| host_error())?;
        if session.locale == crate::localization::UiLocale::System {
            session.locale.activate();
        }
        let control = services.control.lock().map_err(|_| host_error())?;
        Ok(SettingsSnapshot {
            providers: control
                .providers()
                .providers
                .iter()
                .map(ProviderSummary::from)
                .collect(),
            draft: session.draft.clone(),
            has_credential: session
                .credential
                .as_ref()
                .is_some_and(|auth| !auth.is_empty()),
            message: session.message.clone(),
            operation_kind: session.operation_kind,
            busy: session.busy,
            settings_pending: session.settings_running,
            failed: session.failed,
            integration: session.integration.unwrap_or("unknown"),
            theme: session.theme,
            locale: session.locale,
            effective_locale: rust_i18n::locale().to_string(),
            dock_visible: !crate::platform::macos::is_accessory_activation_policy(),
            startup: crate::platform::macos::launch_at_login_status(),
            cancellable: session.cancel.is_some(),
            revision: session.revision,
        })
    }
    pub(crate) fn open(&self, id: &str, services: &AppServices) -> anyhow::Result<()> {
        let mut session = self.0.lock().map_err(|_| host_error())?;
        if session.busy {
            return Ok(());
        }
        let control = services.control.lock().map_err(|_| host_error())?;
        let provider = control
            .providers()
            .providers
            .iter()
            .find(|p| p.id.as_str() == id)
            .ok_or_else(host_error)?;
        session.draft = ProviderDraft {
            id: provider.id.to_string(),
            preset: provider.preset.clone(),
            connection: provider.connection.clone(),
            name: provider.name.clone(),
            endpoint: provider.endpoints.http.clone(),
            protocol: provider.protocol,
            model_endpoint: provider.endpoints.models.clone().unwrap_or_default(),
            websocket_endpoint: provider.endpoints.websocket.clone().unwrap_or_default(),
            websocket: provider.transports.websocket,
            models: provider
                .models
                .iter()
                .map(|m| ModelDraft {
                    id: m.upstream_model_id.to_string(),
                    name: m.display_name.clone(),
                    enabled: m.enabled,
                    context_window: m.context_window,
                    reasoning_levels: m.supported_reasoning_levels.clone(),
                    parallel_tools: m.supports_parallel_tool_calls,
                    search_tool: m.supports_search_tool,
                    metadata_sources: m.metadata_sources.clone(),
                })
                .collect(),
        };
        session.credential = Some(provider.auth.clone());
        session.message.clear();
        session.setting_error = None;
        session.operation_kind = OperationKind::General;
        session.revision += 1;
        Ok(())
    }
    pub(crate) fn edit(&self, draft: ProviderDraft) {
        if let Ok(mut session) = self.0.lock()
            && !session.busy
        {
            session.draft = draft;
            session.message.clear();
            session.setting_error = None;
            session.operation_kind = OperationKind::General;
        }
    }
    pub(crate) fn secret(&self, key: String) {
        if let Ok(mut session) = self.0.lock()
            && !session.busy
        {
            session.credential = Some(AuthConfig::Bearer { api_key: key });
        }
    }
    pub(crate) fn select(&self, family_id: &str, connection_id: &str) -> anyhow::Result<()> {
        let family = provider_x_providers::presets()
            .into_iter()
            .find(|family| family.id == family_id)
            .ok_or_else(|| anyhow::anyhow!("unknown preset"))?;
        let connection = family
            .connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .ok_or_else(|| anyhow::anyhow!("unknown connection"))?;
        let mut session = self.0.lock().map_err(|_| host_error())?;
        if !session.busy {
            let previous = if session.draft.preset == family.id {
                session.draft.clone()
            } else {
                ProviderDraft::default()
            };
            session.draft = ProviderDraft {
                id: previous.id,
                models: previous.models,
                preset: family.id.to_owned(),
                connection: connection.id.to_owned(),
                name: if previous.name.is_empty() {
                    family.name.to_owned()
                } else {
                    previous.name
                },
                endpoint: connection.endpoint.to_owned(),
                protocol: connection.protocol,
                model_endpoint: connection.models_endpoint.unwrap_or_default().to_owned(),
                websocket_endpoint: connection.websocket_endpoint.unwrap_or_default().to_owned(),
                websocket: connection.websocket_endpoint.is_some(),
            };
            session.credential = None;
            session.message.clear();
            session.setting_error = None;
            session.operation_kind = OperationKind::General;
            session.revision += 1;
        }
        Ok(())
    }
    fn setting_action(
        &self,
        kind: SettingKind,
        action: &str,
        services: &AppServices,
        cx: &App,
    ) -> anyhow::Result<()> {
        let mut session = self.0.lock().map_err(|_| host_error())?;
        if session.busy || !session.enqueue_setting(kind, action) {
            return Ok(());
        }
        drop(session);
        let state = self.clone();
        let services = services.clone();
        // Dock changes require the main thread; persistence uses the blocking worker.
        // This task outlives the view and never places the form in a busy state.
        cx.foreground_executor()
            .spawn(async move {
                loop {
                    let action = state.0.lock().ok().and_then(|mut s| s.next_setting());
                    let Some((kind, action)) = action else { break };
                    let result = if kind == SettingKind::Dock {
                        run_action(&action, &state, &services).await
                    } else {
                        let task_state = state.clone();
                        let task_services = services.clone();
                        let task_kind = kind.clone();
                        services
                            .spawn(async move {
                                if let SettingKind::ProviderEnabled(id) = task_kind {
                                    set_provider_enabled(
                                        id,
                                        action == "provider-enable",
                                        &task_services,
                                    )
                                    .await
                                } else {
                                    run_action(&action, &task_state, &task_services).await
                                }
                            })
                            .await
                            .unwrap_or_else(|_| Err(host_error().to_string()))
                    };
                    if let Ok(mut session) = state.0.lock() {
                        session.finish_setting(kind, result);
                    }
                }
            })
            .detach();
        Ok(())
    }

    pub(crate) fn action(
        &self,
        action: &str,
        services: &AppServices,
        cx: &App,
    ) -> anyhow::Result<()> {
        let setting = if matches!(action, "provider-enable" | "provider-disable") {
            let id = self.0.lock().map_err(|_| host_error())?.draft.id.clone();
            Some(SettingKind::ProviderEnabled(ProviderId::new(id)?))
        } else {
            SettingKind::for_action(action)
        };
        if let Some(kind) = setting {
            return self.setting_action(kind, action, services, cx);
        }
        let action = action.to_owned();
        let state = self.clone();
        let services = services.clone();
        let operation;
        {
            let mut session = state.0.lock().map_err(|_| host_error())?;
            if action == "cancel" {
                if let Some(cancel) = session.cancel.take() {
                    cancel.abort();
                    session.operation += 1;
                    session.busy = false;
                    session.message.clear();
                    session.setting_error = None;
                    session.failed = false;
                    session.revision += 1;
                }
                return Ok(());
            }
            if session.busy || session.settings_running {
                return Ok(());
            }
            session.busy = true;
            session.failed = false;
            session.operation += 1;
            operation = session.operation;
            session.message.clear();
            session.setting_error = None;
            session.operation_kind = if action == "login" {
                OperationKind::Login
            } else {
                OperationKind::General
            };
            session.revision += 1;
        }
        let runner = services.clone();
        // The operation outlives the settings window.
        let cancellable = matches!(action.as_str(), "login" | "test" | "discover");
        let completion_state = state.clone();
        let task = async move {
            let result = run_action(&action, &state, &services).await;
            if let Ok(mut session) = completion_state.0.lock() {
                if session.operation != operation {
                    return;
                }
                session.cancel = None;
                session.busy = false;
                session.setting_error = None;
                session.failed = result.is_err();
                session.message = match result {
                    Ok(message) | Err(message) => message,
                };
                session.revision += 1;
            }
        };
        let handle = runner.spawn_cancellable(task);
        if cancellable {
            let mut session = self.0.lock().map_err(|_| host_error())?;
            if session.busy && session.operation == operation {
                session.cancel = Some(handle);
            }
        }
        Ok(())
    }
}

async fn run_action(
    action: &str,
    state: &SettingsState,
    services: &AppServices,
) -> Result<String, String> {
    match action {
        "dock-show" | "dock-hide" => dock_action(action == "dock-show", services).await,
        "theme-system" | "theme-dark" | "theme-light" => {
            let theme = if action == "theme-dark" {
                crate::ui_preferences::ThemePreference::Dark
            } else if action == "theme-light" {
                crate::ui_preferences::ThemePreference::Light
            } else {
                crate::ui_preferences::ThemePreference::System
            };
            let path = crate::control_plane::AppPaths::for_home(
                crate::runtime::data_home().map_err(|e| e.to_string())?,
            )
            .root
            .join("ui-theme.json");
            services
                .spawn_blocking(move || {
                    crate::ui_preferences::save(&path, theme).map_err(|e| e.to_string())
                })
                .await
                .map_err(|_| "task failed")??;
            state.0.lock().map_err(|_| "state unavailable")?.theme = theme;
            Ok(String::new())
        }
        "startup-enable" | "startup-disable" => {
            if cfg!(debug_assertions) && std::env::var_os("PROVIDER_X_TEST_HOME").is_some() {
                return Ok(rust_i18n::t!("settings.isolated_startup").to_string());
            }
            crate::platform::macos::set_launch_at_login(action == "startup-enable")
                .map_err(|e| e.to_string())?;
            Ok(String::new())
        }
        "locale-system" | "locale-en" | "locale-zh-CN" => {
            let locale = crate::localization::UiLocale::from_identifier(
                action.trim_start_matches("locale-"),
            );
            let paths = crate::control_plane::AppPaths::for_home(
                crate::runtime::data_home().map_err(|e| e.to_string())?,
            );
            services
                .spawn_blocking(move || {
                    crate::localization::UiLocaleStore::new(paths.ui_locale)
                        .save(locale)
                        .map_err(|e| e.to_string())
                })
                .await
                .map_err(|_| "task failed")??;
            locale.activate();
            state.0.lock().map_err(|_| "state unavailable")?.locale = locale;
            Ok(String::new())
        }
        "remove" => remove_existing(state, services).await,
        "login" => {
            let preset = state
                .0
                .lock()
                .map_err(|_| "state unavailable")?
                .draft
                .preset
                .clone();
            if preset == "anthropic" {
                return if provider_x_providers::claude_code::executable().is_some() {
                    Ok(rust_i18n::t!("settings.claude_found").to_string())
                } else {
                    Err(rust_i18n::t!("settings.claude_dependency").to_string())
                };
            }
            let auth = services.login_openai_oauth().await?;
            state.0.lock().map_err(|_| "state unavailable")?.credential = Some(auth);
            Ok(rust_i18n::t!("settings.signed_in").to_string())
        }
        "integration-status" | "integration-enable" | "integration-disable" => {
            integration_action(action, state, services).await
        }
        "save" => {
            let provider = compile_draft(state, services)?;
            let task_services = services.clone();

            let outcome = services
                .spawn_blocking(move || {
                    task_services.apply_provider_mutation(
                        crate::control_plane::ControlMutation::SaveProvider(provider),
                    )
                })
                .await
                .map_err(|_| rust_i18n::t!("app.provider.task_failed.save").to_string())??;
            state.0.lock().map_err(|_| "state unavailable")?.draft.id =
                outcome.provider_id.to_string();
            Ok(outcome
                .codex_warning
                .unwrap_or_else(|| rust_i18n::t!("settings.saved").to_string()))
        }
        "discover" | "test" => discover_action(action, state, services).await,
        _ => Err("unknown UI command".to_owned()),
    }
}

async fn dock_action(visible: bool, services: &AppServices) -> Result<String, String> {
    let previous = !crate::platform::macos::is_accessory_activation_policy();
    let path = crate::control_plane::AppPaths::for_home(
        crate::runtime::data_home()
            .map_err(|_| rust_i18n::t!("settings.dock_failed").to_string())?,
    )
    .root
    .join("ui-dock.json");
    crate::platform::macos::set_dock_visible(visible)
        .map_err(|_| rust_i18n::t!("settings.dock_failed").to_string())?;
    let saved = services
        .spawn_blocking(move || crate::ui_preferences::save_dock_visible(&path, visible))
        .await;
    if !matches!(saved, Ok(Ok(()))) {
        // Keep the running application consistent with its saved preference on write failure.
        let _ = crate::platform::macos::set_dock_visible(previous);
        return Err(rust_i18n::t!("settings.dock_failed").to_string());
    }
    Ok(String::new())
}

async fn integration_action(
    action: &str,
    state: &SettingsState,
    services: &AppServices,
) -> Result<String, String> {
    let status = if action == "integration-status" {
        let services = services.clone();
        services
            .clone()
            .spawn_blocking(move || services.codex_status())
            .await
            .map_err(|_| rust_i18n::t!("app.codex.task_failed").to_string())??
    } else {
        services
            .set_codex_integration(action == "integration-enable")
            .await?
    };
    let status = match status.receipt_phase {
        Some(crate::codex_config::ReceiptPhase::Active { .. }) if status.managed_values_match => {
            "active"
        }
        Some(
            crate::codex_config::ReceiptPhase::Active { .. }
            | crate::codex_config::ReceiptPhase::Prepared { .. },
        ) => "changed",
        _ => "inactive",
    };
    state.0.lock().map_err(|_| "state unavailable")?.integration = Some(status);
    Ok(String::new())
}

async fn set_provider_enabled(
    provider_id: ProviderId,
    enabled: bool,
    services: &AppServices,
) -> Result<String, String> {
    let task_services = services.clone();
    let outcome = services
        .spawn_blocking(move || {
            task_services.apply_provider_mutation(
                crate::control_plane::ControlMutation::SetProviderEnabled {
                    provider_id,
                    enabled,
                },
            )
        })
        .await
        .map_err(|_| "task failed")??;
    Ok(outcome.codex_warning.unwrap_or_default())
}

async fn remove_existing(state: &SettingsState, services: &AppServices) -> Result<String, String> {
    let id = ProviderId::new(
        state
            .0
            .lock()
            .map_err(|_| "state unavailable")?
            .draft
            .id
            .clone(),
    )
    .map_err(|e| e.to_string())?;
    let mutation = crate::control_plane::ControlMutation::RemoveProvider(id);
    let task_services = services.clone();
    let outcome = services
        .spawn_blocking(move || task_services.apply_provider_mutation(mutation))
        .await
        .map_err(|_| "task failed")??;
    {
        let mut session = state.0.lock().map_err(|_| "state unavailable")?;
        session.draft = ProviderDraft::default();
        session.credential = None;
    }
    Ok(outcome
        .codex_warning
        .unwrap_or_else(|| rust_i18n::t!("settings.saved").to_string()))
}

async fn discover_action(
    action: &str,
    state: &SettingsState,
    services: &AppServices,
) -> Result<String, String> {
    let provider = compile_draft(state, services)?;
    if matches!(provider.auth, AuthConfig::ClaudeCode) {
        return if provider_x_providers::claude_code::executable().is_some() {
            Ok(rust_i18n::t!("settings.claude_found").to_string())
        } else {
            Err(rust_i18n::t!("settings.claude_dependency").to_string())
        };
    }
    let client = provider_x_catalog::ManualDiscoveryClient::new(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_secs(30),
        8 * 1024 * 1024,
    )
    .map_err(|error| error.to_string())?;
    let profile = provider_x_providers::resolve_provider(&provider);
    let existing = provider_x_core::ProviderModelCache {
        config_fingerprint: profile
            .routing_fingerprint()
            .map_err(|error| error.to_string())?,
        last_successful_refresh_at: String::new(),
        source: profile.model_source(),
        models: provider.models.clone(),
    };
    let outcome = services
        .refresh_provider_models(
            client,
            provider,
            Some(existing),
            chrono::Utc::now().to_rfc3339(),
            action == "discover",
        )
        .await
        .map_err(|_| rust_i18n::t!("settings.discovery_failed").to_string())?;
    let preview = outcome.preview;
    let warning = outcome.registry_warning;
    let matched = outcome.registry_matched_models;
    state.0.lock().map_err(|_| "state unavailable")?.credential = Some(outcome.provider.auth);
    if action == "discover" {
        let mut session = state.0.lock().map_err(|_| "state unavailable")?;
        for model in preview.cache.models {
            let replacement = ModelDraft {
                id: model.upstream_model_id.to_string(),
                name: model.display_name,
                enabled: model.enabled,
                context_window: model.context_window,
                reasoning_levels: model.supported_reasoning_levels,
                parallel_tools: model.supports_parallel_tool_calls,
                search_tool: model.supports_search_tool,
                metadata_sources: model.metadata_sources,
            };
            if let Some(existing) = session
                .draft
                .models
                .iter_mut()
                .find(|item| item.id == replacement.id)
            {
                *existing = replacement;
            } else {
                session.draft.models.push(replacement);
            }
        }
    }

    let mut message = rust_i18n::t!("settings.discovery_ok").to_string();
    if matched > 0 {
        message.push_str(rust_i18n::t!("settings.metadata_matched", count = matched).as_ref());
    }
    if let Some(warning) = warning {
        message.push('\n');
        message.push_str(&warning);
    }
    Ok(message)
}

fn compile_draft(state: &SettingsState, services: &AppServices) -> Result<ProviderConfig, String> {
    let (draft, credential) = {
        let session = state.0.lock().map_err(|_| "state unavailable")?;
        (session.draft.clone(), session.credential.clone())
    };
    let control = services.control.lock().map_err(|_| "state unavailable")?;
    compile_instance(draft, credential, &control.providers().providers)
}

fn compile_instance(
    draft: ProviderDraft,
    credential: Option<AuthConfig>,
    providers: &[ProviderConfig],
) -> Result<ProviderConfig, String> {
    if draft.name.trim().is_empty() {
        return Err(rust_i18n::t!("app.provider.name_required").to_string());
    }
    let family = provider_x_providers::presets()
        .into_iter()
        .find(|preset| preset.id == draft.preset)
        .ok_or("unknown preset")?;
    let connection = family
        .connections
        .iter()
        .find(|connection| connection.id == draft.connection)
        .ok_or("unknown connection")?;
    let id = if draft.id.is_empty() {
        let mut id = draft.preset.clone();
        let mut suffix = 2;
        while providers.iter().any(|provider| provider.id.as_str() == id) {
            id = format!("{}-{suffix}", draft.preset);
            suffix += 1;
        }
        ProviderId::new(id).map_err(|error| error.to_string())?
    } else {
        ProviderId::new(&draft.id).map_err(|error| error.to_string())?
    };
    let saved = providers.iter().find(|provider| provider.id == id);
    let credential = credential.or_else(|| {
        saved
            .filter(|provider| provider.connection == draft.connection)
            .map(|provider| provider.auth.clone())
    });
    let credential = freshest_credential(credential, saved);
    let auth = match (connection.credentials, credential) {
        (provider_x_providers::CredentialKind::ClaudeCode, _) => AuthConfig::ClaudeCode,
        (provider_x_providers::CredentialKind::None, _) => AuthConfig::None,
        (
            provider_x_providers::CredentialKind::OpenaiOAuth,
            Some(auth @ AuthConfig::OpenAiOAuth { .. }),
        )
        | (provider_x_providers::CredentialKind::ApiKey, Some(auth @ AuthConfig::Bearer { .. })) => {
            auth
        }
        (provider_x_providers::CredentialKind::OpenaiOAuth, _) => {
            return Err(rust_i18n::t!("app.provider.oauth.login_required").to_string());
        }
        _ => return Err(rust_i18n::t!("app.provider.api_key_required").to_string()),
    };
    let oauth = matches!(auth, AuthConfig::OpenAiOAuth { .. });
    let models = draft
        .models
        .iter()
        .map(|model| compile_model(model, &id, saved))
        .collect::<Result<Vec<_>, _>>()?;
    let provider = ProviderConfig {
        id: id.clone(),
        name: draft.name.trim().to_owned(),
        description: saved.and_then(|p| p.description.clone()),
        enabled: saved.is_none_or(|p| p.enabled),
        preset: draft.preset,
        connection: draft.connection,
        models,
        protocol: draft.protocol,
        anthropic_thinking: saved.and_then(|provider| provider.anthropic_thinking),
        reasoning_policy: saved.map_or(connection.reasoning_policy, |provider| {
            provider.reasoning_policy
        }),
        endpoints: EndpointConfig {
            http: draft.endpoint,
            websocket: if oauth {
                Some(provider_x_providers::OPENAI_OAUTH_WEBSOCKET_ENDPOINT.to_owned())
            } else if draft.protocol == ProtocolId::OpenaiResponses && draft.websocket {
                Some(draft.websocket_endpoint)
            } else {
                None
            },
            models: if oauth {
                Some(provider_x_providers::OPENAI_OAUTH_MODELS_ENDPOINT.to_owned())
            } else {
                (!draft.model_endpoint.is_empty()).then_some(draft.model_endpoint)
            },
        },
        auth,
        transports: TransportConfig {
            http_sse: true,
            websocket: oauth || (draft.protocol == ProtocolId::OpenaiResponses && draft.websocket),
        },
    };
    provider_x_providers::validate_provider(&provider).map_err(|error| error.to_string())?;
    Ok(provider)
}

fn freshest_credential(
    credential: Option<AuthConfig>,
    saved: Option<&ProviderConfig>,
) -> Option<AuthConfig> {
    if let (
        Some(AuthConfig::OpenAiOAuth {
            account_id,
            expires_at_unix,
            ..
        }),
        Some(provider),
    ) = (&credential, saved)
        && let AuthConfig::OpenAiOAuth {
            account_id: saved_account,
            expires_at_unix: saved_expiry,
            ..
        } = &provider.auth
        && account_id == saved_account
        && saved_expiry > expires_at_unix
    {
        return Some(provider.auth.clone());
    }
    credential
}

fn compile_model(
    model: &ModelDraft,
    id: &ProviderId,
    saved: Option<&ProviderConfig>,
) -> Result<ProviderModelSpec, String> {
    let upstream_model_id = ModelId::new(&model.id).map_err(|error| error.to_string())?;
    let mut configured = saved
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|saved| saved.upstream_model_id == upstream_model_id)
        })
        .cloned()
        .unwrap_or_else(|| ProviderModelSpec {
            catalog_model_id: CatalogModelId::for_provider(id, &upstream_model_id),
            upstream_model_id,
            display_name: model.name.clone(),
            enabled: model.enabled,
            context_window: None,
            supported_reasoning_levels: Vec::new(),
            supports_parallel_tool_calls: None,
            supports_search_tool: None,
            metadata_sources: std::collections::BTreeMap::new(),
        });
    configured
        .metadata_sources
        .clone_from(&model.metadata_sources);
    for (field, changed) in [
        ("display_name", configured.display_name != model.name),
        (
            "context_window",
            configured.context_window != model.context_window,
        ),
        (
            "supported_reasoning_levels",
            configured.supported_reasoning_levels != model.reasoning_levels,
        ),
        (
            "supports_parallel_tool_calls",
            configured.supports_parallel_tool_calls != model.parallel_tools,
        ),
        (
            "supports_search_tool",
            configured.supports_search_tool != model.search_tool,
        ),
    ] {
        if changed && !configured.metadata_sources.contains_key(field) {
            configured.metadata_sources.insert(
                field.to_owned(),
                provider_x_core::MetadataSource::UserConfirmed,
            );
        }
    }
    configured.display_name.clone_from(&model.name);
    configured.enabled = model.enabled;
    configured.context_window = model.context_window;
    configured
        .supported_reasoning_levels
        .clone_from(&model.reasoning_levels);
    configured.supports_parallel_tool_calls = model.parallel_tools;
    configured.supports_search_tool = model.search_tool;
    Ok(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_provider_editor_discards_draft_and_credentials_but_keeps_feedback() {
        let state = SettingsState::default();
        {
            let mut session = state.0.lock().unwrap();
            session.draft.preset = "openai".into();
            session.credential = Some(AuthConfig::Bearer {
                api_key: "synthetic-test-key".into(),
            });
            session.message = "validation failed".into();
            session.busy = true;
        }
        state.clear_draft();
        assert!(state.0.lock().unwrap().credential.is_some());
        state.0.lock().unwrap().busy = false;
        state.clear_draft();
        let session = state.0.lock().unwrap();
        assert!(session.draft.preset.is_empty());
        assert!(session.credential.is_none());
        assert_eq!(session.message, "validation failed");
        assert_eq!(session.revision, 1);
    }

    #[test]
    fn preference_changes_coalesce_without_blocking_the_form() {
        let mut session = Session::default();
        assert!(session.enqueue_setting(SettingKind::Dock, "dock-hide"));
        assert_eq!(
            session.next_setting().map(|(_, action)| action).as_deref(),
            Some("dock-hide")
        );
        assert!(!session.enqueue_setting(SettingKind::Theme, "theme-dark"));
        assert!(!session.enqueue_setting(SettingKind::Theme, "theme-light"));
        assert!(!session.enqueue_setting(SettingKind::Dock, "dock-show"));
        assert!(!session.busy);
        assert!(session.message.is_empty());
        assert_eq!(session.setting_queue.len(), 2);
        assert_eq!(
            session.next_setting().map(|(_, action)| action).as_deref(),
            Some("theme-light")
        );
        assert_eq!(
            session.next_setting().map(|(_, action)| action).as_deref(),
            Some("dock-show")
        );
        let revision = session.revision;
        assert!(session.next_setting().is_none());
        assert!(!session.settings_running);
        assert!(session.revision > revision);
        assert!(session.enqueue_setting(SettingKind::Locale, "locale-en"));
    }

    #[test]
    fn preference_feedback_only_reports_errors_and_clears_a_recovered_failure() {
        let mut session = Session::default();
        session.finish_setting(SettingKind::Dock, Ok("success".into()));
        assert!(session.message.is_empty());
        session.finish_setting(SettingKind::Dock, Err("write failed".into()));
        session.finish_setting(SettingKind::Theme, Ok(String::new()));
        assert_eq!(session.message, "write failed");
        assert!(session.failed);
        session.finish_setting(SettingKind::Dock, Ok(String::new()));
        assert!(session.message.is_empty());
        assert!(!session.failed);
        assert!(!session.busy);
    }

    #[test]
    fn queued_provider_switch_keeps_its_target_when_the_editor_changes() {
        let mut session = Session::default();
        let first = ProviderId::new("first").unwrap();
        let second = ProviderId::new("second").unwrap();
        session.enqueue_setting(
            SettingKind::ProviderEnabled(first.clone()),
            "provider-disable",
        );
        session.draft.id = second.to_string();
        session.enqueue_setting(
            SettingKind::ProviderEnabled(second.clone()),
            "provider-enable",
        );
        assert_eq!(
            session.next_setting(),
            Some((
                SettingKind::ProviderEnabled(first),
                "provider-disable".into()
            ))
        );
        assert_eq!(
            session.next_setting(),
            Some((
                SettingKind::ProviderEnabled(second),
                "provider-enable".into()
            ))
        );
        assert!(!session.busy);
    }

    fn draft(preset: &str, connection_id: &str) -> ProviderDraft {
        let family = provider_x_providers::presets()
            .into_iter()
            .find(|item| item.id == preset)
            .unwrap();
        let connection = family
            .connections
            .iter()
            .find(|item| item.id == connection_id)
            .unwrap();
        ProviderDraft {
            preset: preset.into(),
            connection: connection_id.into(),
            name: family.name.into(),
            endpoint: connection.endpoint.into(),
            protocol: connection.protocol,
            model_endpoint: connection.models_endpoint.unwrap_or_default().into(),
            websocket_endpoint: connection.websocket_endpoint.unwrap_or_default().into(),
            websocket: connection.websocket_endpoint.is_some(),
            models: vec![ModelDraft {
                id: "manual-model".into(),
                name: "Manual".into(),
                enabled: true,
                ..ModelDraft::default()
            }],
            ..ProviderDraft::default()
        }
    }

    fn oauth() -> AuthConfig {
        AuthConfig::OpenAiOAuth {
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            account_id: "synthetic-account".into(),
            email: None,
            expires_at_unix: u64::MAX,
            is_fedramp: false,
        }
    }

    #[test]
    fn preset_mode_changes_keep_models_but_clear_previous_credentials() {
        let state = SettingsState::default();
        state.select("openai", "api").unwrap();
        state.edit(draft("openai", "api"));
        state.secret("synthetic-only".into());
        state.select("openai", "subscription").unwrap();
        {
            let session = state.0.lock().unwrap();
            assert_eq!(session.draft.models.len(), 1);
            assert_eq!(session.draft.connection, "subscription");
            assert!(session.credential.is_none());
        }
        state.select("ollama", "local").unwrap();
        assert!(state.0.lock().unwrap().draft.models.is_empty());
    }

    #[test]
    fn busy_operation_keeps_its_draft_and_credential() {
        let state = SettingsState::default();
        state.select("openai", "api").unwrap();
        state.secret("synthetic-original".into());
        state.0.lock().unwrap().busy = true;
        state.edit(draft("ollama", "local"));
        state.secret("synthetic-replacement".into());
        state.select("ollama", "local").unwrap();
        let session = state.0.lock().unwrap();
        assert_eq!(session.draft.preset, "openai");
        assert!(
            matches!(&session.credential, Some(AuthConfig::Bearer { api_key }) if api_key == "synthetic-original")
        );
    }

    #[test]
    fn all_presets_compile_without_discovery_or_review() {
        for family in provider_x_providers::presets() {
            for connection in family.connections {
                let mut input = draft(family.id, connection.id);
                if input.endpoint.is_empty() && family.id == "custom" {
                    input.endpoint = "http://127.0.0.1:12345/v1".into();
                }
                let credential = match connection.credentials {
                    provider_x_providers::CredentialKind::ApiKey => Some(AuthConfig::Bearer {
                        api_key: "synthetic".into(),
                    }),
                    provider_x_providers::CredentialKind::OpenaiOAuth => Some(oauth()),
                    _ => None,
                };
                let provider = compile_instance(input, credential, &[]).unwrap();
                assert!(provider.models[0].enabled);
                assert_eq!(provider.models[0].context_window, None);
            }
        }
    }

    #[test]
    fn provider_summary_counts_selections_and_bounds_preview_even_when_connection_is_disabled() {
        let mut input = draft("ollama", "local");
        input.models = (0..4)
            .map(|index| ModelDraft {
                id: format!("model-{index}"),
                name: format!("Model {index}"),
                enabled: index != 0,
                ..ModelDraft::default()
            })
            .collect();
        let mut provider = compile_instance(input, None, &[]).unwrap();
        provider.enabled = false;
        let summary = ProviderSummary::from(&provider);
        assert!(!summary.enabled);
        assert_eq!(summary.model_count, 4);
        assert_eq!(summary.enabled_model_count, 3);
        assert_eq!(summary.model_preview, "Model 1 · Model 2 …");
        assert_eq!(summary.auth_label, "local");
        for model in &mut provider.models {
            model.enabled = false;
        }
        let summary = ProviderSummary::from(&provider);
        assert_eq!(summary.model_count, 4);
        assert_eq!(summary.enabled_model_count, 0);
        assert!(summary.model_preview.is_empty());
    }

    #[test]
    fn subscription_cannot_reuse_api_credentials() {
        assert!(
            compile_instance(
                draft("openai", "subscription"),
                Some(AuthConfig::Bearer {
                    api_key: "synthetic".into()
                }),
                &[]
            )
            .is_err()
        );
        assert!(compile_instance(draft("openai", "api"), Some(oauth()), &[]).is_err());
        let claude = compile_instance(draft("anthropic", "subscription"), None, &[]).unwrap();
        assert!(matches!(claude.auth, AuthConfig::ClaudeCode));
        assert_eq!(
            provider_x_providers::resolve_provider(&claude).execution_backend(),
            provider_x_providers::ExecutionBackend::ClaudeCode
        );
    }

    #[test]
    fn edit_preserves_saved_connection_and_marks_manual_metadata() {
        let mut input = draft("ollama", "local");
        input.endpoint = "http://127.0.0.1:32123/v1".into();
        input.model_endpoint = "http://127.0.0.1:32123/custom-models".into();
        let mut existing = compile_instance(input.clone(), None, &[]).unwrap();
        existing.enabled = false;
        existing.description = Some("description".into());
        input.id = existing.id.to_string();
        input.models[0].context_window = Some(8192);
        let edited = compile_instance(input, None, &[existing.clone()]).unwrap();
        assert_eq!(edited.endpoints, existing.endpoints);
        assert!(!edited.enabled);
        assert_eq!(edited.description, existing.description);
        assert_eq!(
            edited.models[0].metadata_sources["context_window"],
            provider_x_core::MetadataSource::UserConfirmed
        );
        let another = compile_instance(draft("ollama", "local"), None, &[existing]).unwrap();
        assert_eq!(another.id.as_str(), "ollama-2");
    }
    #[test]
    fn saving_an_open_draft_does_not_roll_back_refreshed_oauth_tokens() {
        let mut existing =
            compile_instance(draft("openai", "subscription"), Some(oauth()), &[]).unwrap();
        let mut older = oauth();
        if let AuthConfig::OpenAiOAuth {
            expires_at_unix,
            access_token,
            ..
        } = &mut older
        {
            *expires_at_unix = 1;
            *access_token = "synthetic-older".into();
        }
        let refreshed = freshest_credential(Some(older.clone()), Some(&existing)).unwrap();
        assert_eq!(refreshed, existing.auth);
        if let AuthConfig::OpenAiOAuth { account_id, .. } = &mut existing.auth {
            *account_id = "different-account".into();
        }
        assert_eq!(
            freshest_credential(Some(older.clone()), Some(&existing)),
            Some(older)
        );
    }
}
