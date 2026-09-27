use std::sync::{Arc, Mutex};

use gpui_kit::{App, Global};
use gpui_shell::{HostError, HostModule, HostValue};
use provider_x_core::{
    AuthConfig, CatalogModelId, EndpointConfig, ModelId, ProtocolId, ProviderConfig, ProviderId,
    ProviderModelSpec, TransportConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::runtime::AppServices;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ModelDraft {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub parallel_tools: Option<bool>,
    #[serde(default)]
    pub search_tool: Option<bool>,
    #[serde(default)]
    pub metadata_sources: std::collections::BTreeMap<String, provider_x_core::MetadataSource>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct ProviderDraft {
    pub id: String,
    pub preset: String,
    pub connection: String,
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub model_endpoint: String,
    #[serde(default)]
    pub websocket_endpoint: String,
    #[serde(default)]
    pub websocket: bool,
    pub protocol: ProtocolId,
    pub models: Vec<ModelDraft>,
}

#[derive(Default)]
struct Session {
    draft: ProviderDraft,
    credential: Option<AuthConfig>,
    message: String,
    busy: bool,
    failed: bool,
    revision: u64,
    theme: crate::ui_preferences::ThemePreference,
    integration: Option<&'static str>,
    operation: u64,
    cancel: Option<tokio::task::AbortHandle>,
}

#[derive(Clone, Default)]
pub(crate) struct UiHost(Arc<Mutex<Session>>);
impl Global for UiHost {}

impl UiHost {
    pub(crate) fn set_message(&self, message: String) {
        if let Ok(mut session) = self.0.lock() {
            session.message = message;
            session.revision += 1;
        }
    }
}

fn host_error() -> HostError {
    HostError::new(rust_i18n::t!("app.internal.control_lock").to_string())
}

#[allow(clippy::too_many_lines)] // Declarative host-module bindings are kept together.
pub(crate) fn register(cx: &mut App) -> anyhow::Result<()> {
    if !cx.has_global::<UiHost>() {
        let host = UiHost::default();
        let path = crate::control_plane::AppPaths::for_home(crate::runtime::data_home()?)
            .root
            .join("ui-theme.json");
        host.0
            .lock()
            .map_err(|_| anyhow::anyhow!("state unavailable"))?
            .theme = crate::ui_preferences::load(&path)?;
        cx.set_global(host);
    }
    let state = cx.global::<UiHost>().clone();
    let services = cx.global::<AppServices>().clone();
    let snapshot_state = state.clone();
    let snapshot_services = services.clone();
    let edit_state = state.clone();
    let secret_state = state.clone();
    let select_state = state.clone();
    let action_state = state.clone();
    let open_state = state.clone();
    let open_services = services.clone();
    let foreground = cx.foreground_executor().clone();
    gpui_shell::export_module(
        HostModule::new("providerx")
            .function("text", |args| {
                let key = args.string(0)?;
                Ok(HostValue::from(rust_i18n::t!(key).to_string()))
            })
            .function("snapshot", move |_| {
                let session = snapshot_state.0.lock().map_err(|_| host_error())?;
                let control = snapshot_services.control.lock().map_err(|_| host_error())?;
                let providers: Vec<_> = control.providers().providers.iter().map(|provider| {
                    json!({"id": provider.id, "name": provider.name, "enabled": provider.enabled})
                }).collect();
                Ok(HostValue::from(json!({
                    "presets": provider_x_providers::presets(),
                    "providers": providers,
                    "draft": session.draft,
                    "has_credential": session.credential.as_ref().is_some_and(|auth| !auth.is_empty()),
                    "message": session.message,
                    "busy": session.busy,
                    "router_ready": snapshot_services.egress_ready().is_ok(),
                    "integration": session.integration.unwrap_or("unknown"),
                    "failed": session.failed,
                    "theme": session.theme,
                    "dock_visible": !crate::platform::macos::is_accessory_activation_policy(),
                    "cancellable": session.cancel.is_some(),
                    "locale": rust_i18n::locale().to_string(),
                    "startup": format!("{:?}", crate::platform::macos::launch_at_login_status()),
                    "revision": session.revision,
                }).to_string()))
            })
            .function("open", move |args| {
                let id = args.string(0)?;
                let mut session = open_state.0.lock().map_err(|_| host_error())?;
                if session.busy { return Ok(HostValue::Null); }
                let control = open_services.control.lock().map_err(|_| host_error())?;
                let provider = control.providers().providers.iter().find(|p| p.id.as_str() == id)
                    .ok_or_else(host_error)?;
                session.draft = ProviderDraft {
                    id: provider.id.to_string(), preset: provider.preset.clone(),
                    connection: provider.connection.clone(), name: provider.name.clone(),
                    endpoint: provider.endpoints.http.clone(), protocol: provider.protocol,
                    model_endpoint: provider.endpoints.models.clone().unwrap_or_default(), websocket_endpoint: provider.endpoints.websocket.clone().unwrap_or_default(), websocket: provider.transports.websocket,
                    models: provider.models.iter().map(|m| ModelDraft { id: m.upstream_model_id.to_string(), name: m.display_name.clone(), enabled: m.enabled, context_window: m.context_window, reasoning_levels: m.supported_reasoning_levels.clone(), parallel_tools: m.supports_parallel_tool_calls, search_tool: m.supports_search_tool, metadata_sources: m.metadata_sources.clone() }).collect(),
                };
                session.credential = Some(provider.auth.clone());
                session.message.clear();
                session.revision += 1;
                Ok(HostValue::Null)
            })
            .function("edit", move |args| {
                let draft: ProviderDraft = serde_json::from_str(args.string(0)?)
                    .map_err(|_| HostError::new("invalid provider draft"))?;
                let mut session = edit_state.0.lock().map_err(|_| host_error())?;
                if !session.busy {
                    session.draft = draft;
                    session.message.clear();
                }
                Ok(HostValue::Null)
            })
            .function("secret", move |args| {
                let mut session = secret_state.0.lock().map_err(|_| host_error())?;
                if !session.busy {
                    session.credential = Some(AuthConfig::Bearer { api_key: args.string(0)?.to_owned() });
                }
                Ok(HostValue::Null)
            })
            .function("select", move |args| {
                let family_id = args.string(0)?;
                let connection_id = args.string(1)?;
                let family = provider_x_providers::presets().into_iter().find(|family| family.id == family_id)
                    .ok_or_else(|| HostError::new("unknown preset"))?;
                let connection = family.connections.iter().find(|connection| connection.id == connection_id)
                    .ok_or_else(|| HostError::new("unknown connection"))?;
                let mut session = select_state.0.lock().map_err(|_| host_error())?;
                if !session.busy {
                    let previous = if session.draft.preset == family.id { session.draft.clone() } else { ProviderDraft::default() };
                    session.draft = ProviderDraft {
                        id: previous.id, models: previous.models,
                        preset: family.id.to_owned(), connection: connection.id.to_owned(),
                        name: if previous.name.is_empty() { family.name.to_owned() } else { previous.name }, endpoint: connection.endpoint.to_owned(),
                        protocol: connection.protocol,
                        model_endpoint: connection.models_endpoint.unwrap_or_default().to_owned(), websocket_endpoint: connection.websocket_endpoint.unwrap_or_default().to_owned(), websocket: connection.websocket_endpoint.is_some(),
                    };
                    session.credential = None;
                    session.message.clear();
                    session.revision += 1;
                }
                Ok(HostValue::Null)
            })
            .function("action", move |args| {
                let action = args.string(0)?.to_owned();
                let state = action_state.clone();
                let services = services.clone();
                let operation;
                {
                    let mut session = state.0.lock().map_err(|_| host_error())?;
                    if action == "cancel" {
                        if let Some(cancel) = session.cancel.take() {
                            cancel.abort(); session.operation += 1; session.busy = false;
                            session.message = rust_i18n::t!("shell.cancelled").to_string(); session.revision += 1;
                        }
                        return Ok(HostValue::Bool(true));
                    }
                    if session.busy { return Ok(HostValue::Bool(false)); }
                    session.busy = true;
                    session.operation += 1; operation = session.operation;
                    session.message.clear();
                    session.revision += 1;
                }
                let runner = services.clone();
                // The Rust task, not a script promise, owns the operation across reloads.
                let cancellable = matches!(action.as_str(), "login" | "test" | "discover");
                let completion_state = state.clone();
                let needs_main_thread = matches!(action.as_str(), "dock-show" | "dock-hide");
                let task = async move {
                    let result = run_action(&action, &state, &services).await;
                    if let Ok(mut session) = completion_state.0.lock() {
                        if session.operation != operation { return; }
                        session.cancel = None;
                        session.busy = false;
                        session.failed = result.is_err();
                        session.message = match result { Ok(message) | Err(message) => message };
                        session.revision += 1;
                    }
                };
                if needs_main_thread {
                    foreground.spawn(task).detach();
                } else {
                    let handle = runner.spawn_cancellable(task);
                    if cancellable {
                        let mut session = action_state.0.lock().map_err(|_| host_error())?;
                        if session.busy && session.operation == operation { session.cancel = Some(handle); }
                    }
                }
                Ok(HostValue::Bool(true))
            }),
    )?;
    Ok(())
}

async fn run_action(
    action: &str,
    state: &UiHost,
    services: &AppServices,
) -> Result<String, String> {
    match action {
        "dock-show" | "dock-hide" => dock_action(action == "dock-show", services).await,
        "theme-dark" | "theme-light" => {
            let theme = if action == "theme-dark" {
                crate::ui_preferences::ThemePreference::Dark
            } else {
                crate::ui_preferences::ThemePreference::Light
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
            Ok(rust_i18n::t!("shell.preference_saved").to_string())
        }
        "startup-enable" | "startup-disable" => {
            if cfg!(debug_assertions) && std::env::var_os("PROVIDER_X_TEST_HOME").is_some() {
                return Ok(rust_i18n::t!("shell.isolated_startup").to_string());
            }
            crate::platform::macos::set_launch_at_login(action == "startup-enable")
                .map_err(|e| e.to_string())?;
            Ok(rust_i18n::t!("shell.preference_saved").to_string())
        }
        "locale-en" | "locale-zh-CN" => {
            let locale = crate::localization::UiLocale::from_identifier(
                action.trim_start_matches("locale-"),
            );
            let paths = crate::control_plane::AppPaths::for_home(
                crate::runtime::data_home().map_err(|e| e.to_string())?,
            );
            crate::localization::UiLocaleStore::new(paths.ui_locale)
                .save(locale)
                .map_err(|e| e.to_string())?;
            locale.activate();
            Ok(rust_i18n::t!("shell.preference_saved").to_string())
        }
        "remove" | "toggle" => mutate_existing(action, state, services).await,
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
                    Ok(rust_i18n::t!("shell.claude_found").to_string())
                } else {
                    Err(rust_i18n::t!("shell.claude_dependency").to_string())
                };
            }
            let auth = services.login_openai_oauth().await?;
            state.0.lock().map_err(|_| "state unavailable")?.credential = Some(auth);
            Ok(rust_i18n::t!("shell.signed_in").to_string())
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
                .unwrap_or_else(|| rust_i18n::t!("shell.saved").to_string()))
        }
        "discover" | "test" => discover_action(action, state, services).await,
        _ => Err("unknown UI command".to_owned()),
    }
}

async fn dock_action(visible: bool, services: &AppServices) -> Result<String, String> {
    let previous = !crate::platform::macos::is_accessory_activation_policy();
    let path = crate::control_plane::AppPaths::for_home(
        crate::runtime::data_home().map_err(|_| rust_i18n::t!("shell.dock_failed").to_string())?,
    )
    .root
    .join("ui-dock.json");
    crate::platform::macos::set_dock_visible(visible)
        .map_err(|_| rust_i18n::t!("shell.dock_failed").to_string())?;
    let saved = services
        .spawn_blocking(move || crate::ui_preferences::save_dock_visible(&path, visible))
        .await;
    if !matches!(saved, Ok(Ok(()))) {
        // Keep the running application consistent with its saved preference on write failure.
        let _ = crate::platform::macos::set_dock_visible(previous);
        return Err(rust_i18n::t!("shell.dock_failed").to_string());
    }
    Ok(rust_i18n::t!("shell.preference_saved").to_string())
}

async fn integration_action(
    action: &str,
    state: &UiHost,
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
    Ok(if action == "integration-status" {
        String::new()
    } else {
        rust_i18n::t!("shell.integration_hint").to_string()
    })
}

async fn mutate_existing(
    action: &str,
    state: &UiHost,
    services: &AppServices,
) -> Result<String, String> {
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
    let mutation = if action == "remove" {
        crate::control_plane::ControlMutation::RemoveProvider(id)
    } else {
        let control = services.control.lock().map_err(|_| "state unavailable")?;
        let enabled = control
            .providers()
            .providers
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| p.enabled);
        crate::control_plane::ControlMutation::SetProviderEnabled {
            provider_id: id,
            enabled: !enabled,
        }
    };
    let task_services = services.clone();
    let outcome = services
        .spawn_blocking(move || task_services.apply_provider_mutation(mutation))
        .await
        .map_err(|_| "task failed")??;
    if action == "remove" {
        let mut session = state.0.lock().map_err(|_| "state unavailable")?;
        session.draft = ProviderDraft::default();
        session.credential = None;
    }
    Ok(outcome
        .codex_warning
        .unwrap_or_else(|| rust_i18n::t!("shell.saved").to_string()))
}

async fn discover_action(
    action: &str,
    state: &UiHost,
    services: &AppServices,
) -> Result<String, String> {
    let provider = compile_draft(state, services)?;
    if matches!(provider.auth, AuthConfig::ClaudeCode) {
        return if provider_x_providers::claude_code::executable().is_some() {
            Ok(rust_i18n::t!("shell.claude_found").to_string())
        } else {
            Err(rust_i18n::t!("shell.claude_dependency").to_string())
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
        .map_err(|_| rust_i18n::t!("shell.discovery_failed").to_string())?;
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

    let mut message = rust_i18n::t!("shell.discovery_ok").to_string();
    if matched > 0 {
        message.push_str(rust_i18n::t!("shell.metadata_matched", count = matched).as_ref());
    }
    if let Some(warning) = warning {
        message.push('\n');
        message.push_str(&warning);
    }
    Ok(message)
}

fn compile_draft(state: &UiHost, services: &AppServices) -> Result<ProviderConfig, String> {
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
