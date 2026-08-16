use std::{borrow::Cow, cell::Cell, rc::Rc, time::Duration};

use chrono::{SecondsFormat, Utc};
use gpui::{
    App, AssetSource, Bounds, Context, Entity, Global, PromptButton, PromptLevel, Render,
    SharedString, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, div, img,
    prelude::*, px, size,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, IconNamed, IndexPath, Root, Selectable, Sizable,
    StyledExt, Theme, WindowExt,
    button::{Button, ButtonVariants},
    dialog::{DialogAction, DialogClose, DialogFooter},
    input::{Input, InputEvent, InputState},
    link::Link,
    scroll::ScrollableElement,
    select::{Select, SelectEvent, SelectState},
    switch::Switch,
};
use provider_x_catalog::{
    ManualDiscoveryClient, ModelCapabilitySettings, RefreshPreview, set_model_enabled,
    update_model_capabilities,
};
use provider_x_core::{
    AnthropicThinkingMode, AuthConfig, EndpointConfig, ModelId, ModelPublicationStatus, ProtocolId,
    ProviderConfig, ProviderId, ProviderModelSpec, TransportConfig,
};

use crate::codex_config::{CodexConfigStatus, ReceiptPhase};
use crate::control_plane::{ControlMutation, ControlPlane};
use crate::localization::{ENGLISH_LABEL, SIMPLIFIED_CHINESE_LABEL, UiLocale, UiLocaleStore};
use crate::platform::macos::{
    LaunchAtLoginStatus, is_accessory_activation_policy, launch_at_login_status,
    set_accessory_activation_policy, set_launch_at_login,
    tray::{MacTrayController, TrayCommand},
};
use crate::runtime::AppServices;
use crate::{control_plane::AppPaths, storage::SingleInstanceGuard};

const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(40);
const DISCOVERY_BODY_LIMIT: usize = 8 * 1024 * 1024;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEEPSEEK_PROVIDER: &str = "DeepSeek";
const RESPONSES_PROTOCOL: &str = "Responses";
const CHAT_COMPLETIONS_PROTOCOL: &str = "Chat Completions";
const ANTHROPIC_MESSAGES_PROTOCOL: &str = "Anthropic Messages";
const GITHUB_URL: &str = "https://github.com/brookqin/provider-x";
const REFRESH_MODELS_ICON_PATH: &str = "provider-x/refresh-models.svg";
const SETTINGS_APP_ICON_LIGHT_PATH: &str = "provider-x/app-icon-light.png";
const SETTINGS_APP_ICON_DARK_PATH: &str = "provider-x/app-icon-dark.png";
const TEXT_SIZE_PAGE_TITLE: f32 = 24.0;
const TEXT_SIZE_DIALOG_TITLE: f32 = 18.0;
const TEXT_SIZE_BRAND_TITLE: f32 = 16.0;
const TEXT_SIZE_BODY: f32 = 13.0;
const TEXT_SIZE_CAPTION: f32 = 12.0;
const EMBEDDED_ASSETS: &[(&str, &[u8])] = &[
    (
        "icons/chevron-down.svg",
        include_bytes!("../resources/icons/chevron-down.svg"),
    ),
    (
        "icons/circle-x.svg",
        include_bytes!("../resources/icons/circle-x.svg"),
    ),
    (
        "icons/close.svg",
        include_bytes!("../resources/icons/close.svg"),
    ),
    (
        "icons/eye-off.svg",
        include_bytes!("../resources/icons/eye-off.svg"),
    ),
    (
        "icons/eye.svg",
        include_bytes!("../resources/icons/eye.svg"),
    ),
    (
        "icons/github.svg",
        include_bytes!("../resources/icons/github.svg"),
    ),
    (
        "icons/inbox.svg",
        include_bytes!("../resources/icons/inbox.svg"),
    ),
    (
        "icons/minus.svg",
        include_bytes!("../resources/icons/minus.svg"),
    ),
    (
        "icons/plus.svg",
        include_bytes!("../resources/icons/plus.svg"),
    ),
    (
        "icons/search.svg",
        include_bytes!("../resources/icons/search.svg"),
    ),
    (
        "icons/settings-2.svg",
        include_bytes!("../resources/icons/settings-2.svg"),
    ),
    (
        REFRESH_MODELS_ICON_PATH,
        include_bytes!("../resources/icons/refresh-models.svg"),
    ),
    (
        SETTINGS_APP_ICON_LIGHT_PATH,
        include_bytes!("../resources/app-icon/settings-light.png"),
    ),
    (
        SETTINGS_APP_ICON_DARK_PATH,
        include_bytes!("../resources/app-icon/settings-dark.png"),
    ),
];

macro_rules! tr {
    ($key:literal) => {
        rust_i18n::t!($key).to_string()
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        rust_i18n::t!($key, $($name = $value),+).to_string()
    };
}

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        EMBEDDED_ASSETS
            .iter()
            .find_map(|(asset_path, bytes)| {
                (*asset_path == path).then_some(Some(Cow::Borrowed(*bytes)))
            })
            .ok_or_else(|| anyhow::anyhow!("could not find asset at path {path:?}"))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(EMBEDDED_ASSETS
            .iter()
            .filter(|(asset_path, _)| asset_path.starts_with(path))
            .map(|(asset_path, _)| (*asset_path).into())
            .collect())
    }
}

#[derive(Clone, Copy)]
enum AppIcon {
    RefreshModels,
}

impl IconNamed for AppIcon {
    fn path(self) -> SharedString {
        match self {
            Self::RefreshModels => REFRESH_MODELS_ICON_PATH.into(),
        }
    }
}

struct TrayControllerGlobal(Rc<MacTrayController>);

impl Global for TrayControllerGlobal {}

#[derive(Clone)]
struct UiLocaleState {
    current: UiLocale,
    store: UiLocaleStore,
}

impl Global for UiLocaleState {}

#[derive(Default)]
struct SettingsRegistry {
    view: Option<WeakEntity<SettingsView>>,
}

impl Global for SettingsRegistry {}

#[derive(Clone, Copy, Debug, Default)]
struct LaunchOptions {
    show_settings: bool,
    smoke_lifecycle: bool,
    smoke_lock_only: bool,
    smoke_exit_after: Option<Duration>,
}

impl LaunchOptions {
    fn from_args() -> anyhow::Result<Self> {
        let mut options = Self::default();
        for argument in std::env::args().skip(1) {
            if argument == "--show-settings" {
                options.show_settings = true;
            } else if argument == "--smoke-lifecycle" {
                options.smoke_lifecycle = true;
            } else if argument == "--smoke-lock-only" {
                options.smoke_lock_only = true;
            } else if let Some(value) = argument.strip_prefix("--smoke-exit-after-ms=") {
                let milliseconds = value.parse::<u64>()?;
                options.smoke_exit_after = Some(Duration::from_millis(milliseconds));
            } else {
                anyhow::bail!("unknown argument: {argument}");
            }
        }
        Ok(options)
    }
}

/// Starts the macOS tray application and runs until an explicit quit command.
///
/// # Errors
///
/// Returns an error for invalid launch arguments or a failed AppKit/tray/window initialization.
pub fn run() -> anyhow::Result<()> {
    let options = LaunchOptions::from_args()?;
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let paths = AppPaths::for_home(std::path::PathBuf::from(home));
    if options.smoke_lock_only {
        let _guard = SingleInstanceGuard::acquire(paths.root.join("provider-x.lock"))?;
        return Ok(());
    }
    let locale_store = UiLocaleStore::new(paths.ui_locale);
    let locale = match locale_store.load() {
        Ok(Some(locale)) => locale,
        Ok(None) => UiLocale::system_default(),
        Err(error) => {
            eprintln!("failed to load UI locale preference: {error}");
            UiLocale::system_default()
        }
    };
    locale.activate();
    let startup_error = Rc::new(std::cell::RefCell::new(None));
    let reported_error = Rc::clone(&startup_error);

    gpui_platform::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            if let Err(error) = launch(cx, options, locale, locale_store.clone()) {
                *reported_error.borrow_mut() = Some(error);
                cx.quit();
            }
        });

    if let Some(error) = startup_error.borrow_mut().take() {
        Err(error)
    } else {
        Ok(())
    }
}

fn launch(
    cx: &mut App,
    options: LaunchOptions,
    locale: UiLocale,
    locale_store: UiLocaleStore,
) -> anyhow::Result<()> {
    rust_i18n::extend!(gpui_component);
    gpui_component::init(cx);
    cx.set_global(UiLocaleState {
        current: locale,
        store: locale_store,
    });
    let services = if options.smoke_lifecycle {
        AppServices::new_with_listener_port(Some(0))?
    } else {
        AppServices::new()?
    };
    services.egress_ready().map_err(anyhow::Error::msg)?;
    let codex_enabled = services
        .codex_status()
        .as_ref()
        .is_ok_and(codex_integration_is_active);
    println!(
        "PROVIDER_X_SMOKE egress=ready address={}",
        services.egress.address
    );
    let listener_address = services.egress.address;
    cx.set_global(services);
    set_accessory_activation_policy()?;
    anyhow::ensure!(
        is_accessory_activation_policy(),
        "macOS activation policy did not remain Accessory"
    );

    let tray = Rc::new(MacTrayController::new(listener_address, codex_enabled)?);
    cx.set_global(TrayControllerGlobal(Rc::clone(&tray)));
    cx.set_global(SettingsRegistry::default());
    println!("PROVIDER_X_SMOKE tray=ready activation_policy=accessory");

    if options.show_settings {
        open_or_focus_settings(cx)?;
    }

    let tray_task = Rc::clone(&tray);
    cx.spawn(async move |cx| {
        loop {
            while let Some(command) = tray_task.next_command() {
                let should_continue = cx.update(|cx| match command {
                    TrayCommand::OpenSettings => {
                        if let Err(error) = open_or_focus_settings(cx) {
                            let message = format!("failed to open settings: {error:#}");
                            cx.global::<AppServices>()
                                .record_runtime_error("open_settings_failed", &message);
                            eprintln!("{message}");
                        }
                        true
                    }
                    TrayCommand::ManageCodexIntegration => {
                        manage_codex_integration_from_tray(cx, Rc::clone(&tray_task));
                        true
                    }
                    TrayCommand::Quit => {
                        graceful_quit(cx);
                        false
                    }
                });
                if !should_continue {
                    return;
                }
            }
            cx.background_executor().timer(TRAY_POLL_INTERVAL).await;
        }
    })
    .detach();

    if let Some(delay) = options.smoke_exit_after {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            cx.update(|cx| {
                println!("PROVIDER_X_SMOKE lifecycle=quit");
                graceful_quit(cx);
            });
        })
        .detach();
    }

    if options.smoke_lifecycle {
        spawn_smoke_lifecycle(cx);
    }

    Ok(())
}

fn spawn_smoke_lifecycle(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(600))
            .await;
        cx.update(|cx| {
            for handle in cx.windows() {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        });
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
        cx.update(|cx| {
            if cx.windows().is_empty() {
                println!("PROVIDER_X_SMOKE lifecycle=window_closed_process_alive");
                if let Err(error) = open_or_focus_settings(cx) {
                    eprintln!("failed to reopen settings: {error:#}");
                    cx.quit();
                } else {
                    println!("PROVIDER_X_SMOKE lifecycle=window_reopened");
                }
            } else {
                eprintln!("settings window did not close during lifecycle smoke");
                cx.quit();
            }
        });
    })
    .detach();
}

fn manage_codex_integration_from_tray(cx: &mut App, tray: Rc<MacTrayController>) {
    let services = cx.global::<AppServices>().clone();
    let enabled = services
        .codex_status()
        .as_ref()
        .is_ok_and(codex_integration_is_active);
    if !enabled {
        if let Err(error) = open_or_focus_settings(cx) {
            let message = format!("failed to open settings for Codex integration: {error:#}");
            services.record_runtime_error("open_codex_settings_failed", &message);
            eprintln!("{message}");
        }
        return;
    }

    tray.set_codex_operation_pending(false);
    let task_services = services.clone();
    let status_services = services.clone();
    let receiver = services.spawn(async move { task_services.set_codex_integration(false).await });
    cx.spawn(async move |cx| {
        let result = receiver
            .await
            .unwrap_or_else(|_| Err(tr!("app.codex.task_failed")));
        cx.update(|cx| match result {
            Ok(status) => {
                let enabled = codex_integration_is_active(&status);
                tray.set_codex_enabled(enabled);
                sync_settings_codex_status(cx, &status, &tr!("app.codex.restored_tray"), true);
            }
            Err(error) => {
                let diagnostic = redacted_codex_disable_diagnostic(&error);
                status_services.record_runtime_error("codex_disable_failed", diagnostic);
                eprintln!("{diagnostic}");
                if let Ok(status) = status_services.codex_status() {
                    tray.set_codex_enabled(codex_integration_is_active(&status));
                    sync_settings_codex_status(cx, &status, &error, false);
                } else {
                    tray.set_codex_enabled(true);
                    sync_settings_codex_error(cx, error);
                }
                if let Err(open_error) = open_or_focus_settings(cx) {
                    let message = format!(
                        "failed to open settings after Codex integration error: {open_error:#}"
                    );
                    status_services
                        .record_runtime_error("open_settings_after_codex_error_failed", &message);
                    eprintln!("{message}");
                }
            }
        });
    })
    .detach();
}

fn sync_settings_codex_status(
    cx: &mut App,
    status: &CodexConfigStatus,
    message: &str,
    success: bool,
) {
    let settings = cx.global::<SettingsRegistry>().view.clone();
    if let Some(settings) = settings {
        let _ = settings.update(cx, |view, cx| {
            view.apply_codex_status(status);
            view.operation = OperationState::Message {
                success,
                text: message.to_owned(),
            };
            cx.notify();
        });
    }
}

fn redacted_codex_disable_diagnostic(_error: &str) -> &'static str {
    "failed to disable Codex integration; open settings for details"
}

fn sync_settings_codex_error(cx: &mut App, error: String) {
    let settings = cx.global::<SettingsRegistry>().view.clone();
    if let Some(settings) = settings {
        let _ = settings.update(cx, |view, cx| {
            view.operation = OperationState::Message {
                success: false,
                text: error,
            };
            cx.notify();
        });
    }
}

fn graceful_quit(cx: &mut App) {
    let services = cx.global::<AppServices>().clone();
    let shutdown_services = services.clone();
    let log_services = services.clone();
    let receiver = services.spawn(async move { shutdown_services.shutdown_egress().await });
    cx.spawn(async move |cx| {
        let result = receiver
            .await
            .unwrap_or_else(|_| Err(tr!("app.internal.egress_exit_task")));
        cx.update(|cx| {
            if let Err(error) = result {
                log_services.record_runtime_error("egress_shutdown_failed", &error);
                eprintln!("{error}");
            }
            cx.quit();
        });
    })
    .detach();
}

fn open_or_focus_settings(cx: &mut App) -> anyhow::Result<()> {
    if let Some(handle) = cx.windows().first().copied() {
        cx.activate(true);
        handle.update(cx, |_, window, _| window.activate_window())?;
        return Ok(());
    }

    let bounds = Bounds::centered(None, size(px(980.0), px(720.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.0), px(640.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("ProviderX".into()),
                ..TitlebarOptions::default()
            }),
            ..WindowOptions::default()
        },
        |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            window
                .observe_window_appearance(|window, cx| {
                    Theme::sync_system_appearance(Some(window), cx);
                })
                .detach();
            let services = cx.global::<AppServices>().clone();
            let tray = Rc::clone(&cx.global::<TrayControllerGlobal>().0);
            let settings = cx.new(|cx| SettingsView::new(services, tray, window, cx));
            cx.set_global(SettingsRegistry {
                view: Some(settings.downgrade()),
            });
            cx.new(|cx| Root::new(settings, window, cx))
        },
    )?;
    cx.activate(true);
    println!("PROVIDER_X_SMOKE settings_window=open");
    Ok(())
}

fn codex_integration_is_active(status: &CodexConfigStatus) -> bool {
    matches!(status.receipt_phase, Some(ReceiptPhase::Active { .. })) && status.managed_values_match
}

fn current_codex_integration_is_active(services: &AppServices) -> bool {
    services
        .codex_status()
        .as_ref()
        .is_ok_and(codex_integration_is_active)
}

struct SettingsView {
    services: AppServices,
    tray: Rc<MacTrayController>,
    provider_select: Entity<SelectState<Vec<String>>>,
    language_select: Entity<SelectState<Vec<&'static str>>>,
    provider_name: Entity<InputState>,
    http_url: Entity<InputState>,
    websocket_url: Entity<InputState>,
    api_key: Entity<InputState>,
    api_key_visibility: ApiKeyVisibility,
    protocol_select: Entity<SelectState<Vec<&'static str>>>,
    model_search: Entity<InputState>,
    model_display_name: Entity<InputState>,
    model_context_window: Entity<InputState>,
    model_reasoning_levels: Entity<InputState>,
    protocol: ProtocolId,
    websocket_enabled: bool,
    model_parallel_tools: bool,
    model_search_tool: bool,
    provider_template: ProviderTemplate,
    providers: Vec<ProviderSummary>,
    selected_page: SettingsPage,
    editing_provider: Option<ProviderId>,
    operation: OperationState,
    preview: Option<(ProviderConfig, RefreshPreview)>,
    reviewing_model: Option<ModelId>,
    codex_integration: CodexIntegrationUi,
    launch_at_login: LaunchAtLoginStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum SettingsPage {
    #[default]
    Global,
    Provider(ProviderId),
    NewProvider,
}

#[derive(Clone, Copy, Debug, Default)]
struct CodexIntegrationUi {
    enabled: bool,
}

#[derive(Clone, Debug)]
struct ProviderSummary {
    id: ProviderId,
    name: String,
    enabled: bool,
    ready_models: usize,
    review_models: usize,
}

#[derive(Clone, Debug, Default)]
enum OperationState {
    #[default]
    Idle,
    Busy(String),
    Message {
        success: bool,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ApiKeyVisibility {
    #[default]
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProviderTemplate {
    #[default]
    DeepSeek,
    Custom,
}

impl SettingsView {
    fn new(
        services: AppServices,
        tray: Rc<MacTrayController>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let providers = provider_summaries(&services);
        let codex_enabled = current_codex_integration_is_active(&services);
        let provider_select = new_provider_select(window, cx);
        let language_select = new_language_select(cx.global::<UiLocaleState>().current, window, cx);
        let protocol_select = new_protocol_select(window, cx);
        let model_search = new_model_search(window, cx);
        let provider_subscription = cx.subscribe_in(
            &provider_select,
            window,
            |view, _, event: &SelectEvent<Vec<String>>, window, cx| {
                let SelectEvent::Confirm(Some(provider)) = event else {
                    return;
                };
                if provider == DEEPSEEK_PROVIDER {
                    view.apply_deepseek_template(window, cx);
                } else {
                    view.apply_custom_template(window, cx);
                }
            },
        );
        let language_subscription = cx.subscribe_in(
            &language_select,
            window,
            |view, _, event: &SelectEvent<Vec<&'static str>>, window, cx| {
                let SelectEvent::Confirm(Some(label)) = event else {
                    return;
                };
                if let Some(locale) = UiLocale::from_label(label) {
                    view.set_ui_locale(locale, window, cx);
                }
            },
        );
        let protocol_subscription = cx.subscribe_in(
            &protocol_select,
            window,
            |view, _, event: &SelectEvent<Vec<&'static str>>, window, cx| {
                let SelectEvent::Confirm(Some(protocol)) = event else {
                    return;
                };
                view.apply_protocol_selection(protocol, window, cx);
            },
        );
        let view = Self {
            services,
            tray,
            provider_select,
            language_select,
            provider_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("app.provider.placeholder.name"))
            }),
            http_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder("https://gateway.example.com/v1")
            }),
            websocket_url: cx.new(|cx| {
                InputState::new(window, cx).placeholder("wss://gateway.example.com/v1/responses")
            }),
            api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("API Key")
                    .masked(true)
            }),
            api_key_visibility: ApiKeyVisibility::Hidden,
            protocol_select,
            model_search,
            model_display_name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("app.model.display_name_placeholder"))
            }),
            model_context_window: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("app.model.context_placeholder"))
            }),
            model_reasoning_levels: cx.new(|cx| {
                InputState::new(window, cx).placeholder(tr!("app.model.reasoning_placeholder"))
            }),
            protocol: ProtocolId::OpenaiResponses,
            websocket_enabled: false,
            model_parallel_tools: false,
            model_search_tool: false,
            provider_template: ProviderTemplate::DeepSeek,
            providers,
            selected_page: SettingsPage::Global,
            editing_provider: None,
            operation: OperationState::Idle,
            preview: None,
            reviewing_model: None,
            codex_integration: CodexIntegrationUi {
                enabled: codex_enabled,
            },
            launch_at_login: launch_at_login_status(),
        };
        provider_subscription.detach();
        language_subscription.detach();
        protocol_subscription.detach();
        view
    }

    fn apply_protocol_selection(
        &mut self,
        protocol: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.protocol = match protocol {
            CHAT_COMPLETIONS_PROTOCOL => ProtocolId::OpenaiChatCompletions,
            ANTHROPIC_MESSAGES_PROTOCOL => ProtocolId::AnthropicMessages,
            _ => ProtocolId::OpenaiResponses,
        };
        if self.protocol != ProtocolId::OpenaiResponses {
            self.websocket_enabled = false;
        }
        if self.provider_template == ProviderTemplate::DeepSeek {
            let endpoint = if self.protocol == ProtocolId::AnthropicMessages {
                "https://api.deepseek.com/anthropic"
            } else {
                "https://api.deepseek.com"
            };
            self.http_url
                .update(cx, |input, cx| input.set_value(endpoint, window, cx));
        }
        self.preview = None;
        cx.notify();
    }

    fn set_ui_locale(&mut self, locale: UiLocale, window: &mut Window, cx: &mut Context<Self>) {
        let state = cx.global::<UiLocaleState>().clone();
        if state.current == locale {
            return;
        }
        if let Err(error) = state.store.save(locale) {
            self.operation = OperationState::Message {
                success: false,
                text: tr!("app.language.save_failed", error = error),
            };
            self.language_select.update(cx, |select, cx| {
                select.set_selected_value(&state.current.label(), window, cx);
            });
            cx.notify();
            return;
        }

        locale.activate();
        cx.global_mut::<UiLocaleState>().current = locale;
        self.provider_select.update(cx, |select, cx| {
            select.set_items(provider_template_options(), window, cx);
            let selected = provider_template_label(self.provider_template);
            select.set_selected_value(&selected, window, cx);
        });
        self.provider_name.update(cx, |input, cx| {
            input.set_placeholder(tr!("app.provider.placeholder.name"), window, cx);
        });
        self.model_display_name.update(cx, |input, cx| {
            input.set_placeholder(tr!("app.model.display_name_placeholder"), window, cx);
        });
        self.model_context_window.update(cx, |input, cx| {
            input.set_placeholder(tr!("app.model.context_placeholder"), window, cx);
        });
        self.model_reasoning_levels.update(cx, |input, cx| {
            input.set_placeholder(tr!("app.model.reasoning_placeholder"), window, cx);
        });
        self.model_search.update(cx, |input, cx| {
            input.set_placeholder(tr!("app.model.search_placeholder"), window, cx);
        });
        self.tray.refresh_locale();
        self.operation = OperationState::Message {
            success: true,
            text: tr!("app.language.changed", language = locale.label()),
        };
        cx.notify();
    }

    fn is_busy(&self) -> bool {
        match &self.operation {
            OperationState::Idle => false,
            OperationState::Busy(text) => {
                let _ = text;
                true
            }
            OperationState::Message { success, text } => {
                let _ = (success, text);
                false
            }
        }
    }

    fn apply_codex_status(&mut self, status: &CodexConfigStatus) {
        self.codex_integration.enabled = codex_integration_is_active(status);
        self.tray.set_codex_enabled(self.codex_integration.enabled);
    }

    fn apply_deepseek_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_template = ProviderTemplate::DeepSeek;
        if self.editing_provider.is_some() {
            return;
        }
        self.provider_name
            .update(cx, |input, cx| input.set_value("DeepSeek", window, cx));
        self.http_url.update(cx, |input, cx| {
            input.set_value("https://api.deepseek.com", window, cx);
        });
        self.websocket_url
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.protocol = ProtocolId::OpenaiChatCompletions;
        self.protocol_select.update(cx, |select, cx| {
            select.set_selected_value(&CHAT_COMPLETIONS_PROTOCOL, window, cx);
        });
        self.websocket_enabled = false;
        self.preview = None;
        self.operation = OperationState::Idle;
        cx.notify();
    }

    fn apply_custom_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider_template = ProviderTemplate::Custom;
        if self.editing_provider.is_some() {
            return;
        }
        for input in [
            &self.provider_name,
            &self.http_url,
            &self.websocket_url,
            &self.api_key,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.api_key
            .update(cx, |input, cx| input.set_masked(true, window, cx));
        self.api_key_visibility = ApiKeyVisibility::Hidden;
        self.protocol = ProtocolId::OpenaiResponses;
        self.protocol_select.update(cx, |select, cx| {
            select.set_selected_value(&RESPONSES_PROTOCOL, window, cx);
        });
        self.websocket_enabled = false;
        self.preview = None;
        self.operation = OperationState::Idle;
        cx.notify();
    }

    fn draft_provider(&self, cx: &App) -> anyhow::Result<ProviderConfig> {
        let name = self.provider_name.read(cx).value().trim().to_owned();
        anyhow::ensure!(!name.is_empty(), tr!("app.provider.name_required"));
        let id = if let Some(editing) = self.editing_provider.as_ref() {
            editing.clone()
        } else {
            let base = match self.provider_template {
                ProviderTemplate::DeepSeek => "deepseek".to_owned(),
                ProviderTemplate::Custom => provider_namespace_from_name(&name)?,
            };
            ProviderId::new(next_available_provider_id(
                &base,
                self.providers.iter().map(|provider| provider.id.as_str()),
            ))?
        };
        let websocket = self.websocket_url.read(cx).value();
        let entered_api_key = self.api_key.read(cx).unmask_value().to_string();
        anyhow::ensure!(
            !entered_api_key.trim().is_empty(),
            tr!("app.provider.api_key_required")
        );
        let existing = if let Some(editing) = self.editing_provider.as_ref() {
            let control = self
                .services
                .control
                .lock()
                .map_err(|_| anyhow::anyhow!(tr!("app.internal.control_lock")))?;
            Some(
                control
                    .providers()
                    .providers
                    .iter()
                    .find(|provider| &provider.id == editing)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!(tr!("app.provider.missing_edited")))?,
            )
        } else {
            None
        };
        let auth = AuthConfig::Bearer {
            api_key: entered_api_key,
        };
        let enabled = existing.as_ref().is_some_and(|provider| provider.enabled);
        let anthropic_thinking = (self.protocol == ProtocolId::AnthropicMessages).then(|| {
            existing
                .as_ref()
                .and_then(|provider| provider.anthropic_thinking)
                .unwrap_or(if self.provider_template == ProviderTemplate::DeepSeek {
                    AnthropicThinkingMode::Enabled
                } else {
                    AnthropicThinkingMode::Adaptive
                })
        });
        let provider = ProviderConfig {
            id,
            name,
            description: None,
            enabled,
            protocol: self.protocol,
            anthropic_thinking,
            endpoints: EndpointConfig {
                http: self.http_url.read(cx).value().trim().to_owned(),
                websocket: (self.protocol == ProtocolId::OpenaiResponses
                    && !websocket.trim().is_empty())
                .then(|| websocket.trim().to_owned()),
                models: (self.provider_template == ProviderTemplate::DeepSeek)
                    .then(|| "https://api.deepseek.com/models".to_owned()),
            },
            auth,
            transports: TransportConfig {
                http_sse: true,
                websocket: self.protocol == ProtocolId::OpenaiResponses && self.websocket_enabled,
            },
        };
        provider.validate()?;
        Ok(provider)
    }

    fn start_refresh(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let provider = match self.draft_provider(cx) {
            Ok(provider) => provider,
            Err(error) => {
                self.operation = OperationState::Message {
                    success: false,
                    text: error.to_string(),
                };
                cx.notify();
                return;
            }
        };
        let existing = self
            .services
            .control
            .lock()
            .ok()
            .and_then(|control| control.cache().providers.get(&provider.id).cloned());
        let client = match ManualDiscoveryClient::new(
            Duration::from_secs(10),
            Duration::from_secs(30),
            DISCOVERY_BODY_LIMIT,
        ) {
            Ok(client) => client,
            Err(error) => {
                self.operation = OperationState::Message {
                    success: false,
                    text: error.to_string(),
                };
                cx.notify();
                return;
            }
        };
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let services = self.services.clone();
        let refresh_provider = provider.clone();
        let receiver = self.services.spawn(async move {
            services
                .refresh_provider_models(client, refresh_provider, existing, timestamp)
                .await
        });
        self.operation = OperationState::Busy(tr!("app.provider.busy.refresh"));
        self.preview = None;
        self.reviewing_model = None;
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(tr!("app.provider.task_failed.refresh")));
            if let Some(this) = this.upgrade() {
                this.update(cx, |view, cx| {
                    match result {
                        Ok(outcome) => {
                            let preview = outcome.preview;
                            let total = preview.cache.models.len();
                            let registry = outcome.registry_matched_models;
                            view.preview = Some((provider, preview));
                            view.reviewing_model = None;
                            view.operation = OperationState::Message {
                                success: outcome.registry_warning.is_none(),
                                text: outcome.registry_warning.map_or_else(
                                    || {
                                        tr!(
                                            "app.provider.refresh_success",
                                            total = total,
                                            registry = registry
                                        )
                                    },
                                    |warning| {
                                        tr!(
                                            "app.provider.refresh_warning",
                                            total = total,
                                            warning = warning
                                        )
                                    },
                                ),
                            };
                        }
                        Err(error) => {
                            view.operation = OperationState::Message {
                                success: false,
                                text: error,
                            };
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn commit_preview(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some((provider, preview)) = self.preview.take() else {
            return;
        };
        self.reviewing_model = None;
        let retry_preview = (provider.clone(), preview.clone());
        let services = self.services.clone();
        let task_services = services.clone();
        let receiver = services.spawn_blocking(move || {
            let outcome = task_services
                .apply_provider_mutation(ControlMutation::CommitRefresh { provider, preview })?;
            Ok::<_, String>((
                outcome.provider_id,
                provider_summaries(&task_services),
                outcome.codex_warning,
            ))
        });
        self.operation = OperationState::Busy(tr!("app.provider.busy.save_models"));
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(tr!("app.provider.task_failed.save_models")));
            if let Some(this) = this.upgrade() {
                this.update(cx, |view, cx| {
                    match result {
                        Ok((provider_id, providers, warning)) => {
                            view.providers = providers;
                            view.editing_provider = Some(provider_id.clone());
                            view.selected_page = SettingsPage::Provider(provider_id);
                            view.operation = OperationState::Message {
                                success: warning.is_none(),
                                text: warning.map_or_else(
                                    || {
                                        tr!(
                                            "app.provider.saved_models",
                                            restart_hint = tr!("app.restart_hint")
                                        )
                                    },
                                    |warning| {
                                        tr!("app.provider.saved_models_warning", warning = warning)
                                    },
                                ),
                            };
                        }
                        Err(error) => {
                            view.preview = Some(retry_preview);
                            view.operation = OperationState::Message {
                                success: false,
                                text: error,
                            };
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn save_provider(&mut self, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let provider = match self.draft_provider(cx) {
            Ok(provider) => provider,
            Err(error) => {
                self.operation = OperationState::Message {
                    success: false,
                    text: error.to_string(),
                };
                cx.notify();
                return;
            }
        };
        if let Some((preview_provider, _)) = self.preview.as_mut()
            && preview_provider.id == provider.id
        {
            *preview_provider = provider.clone();
        }
        let services = self.services.clone();
        let task_services = services.clone();
        let receiver = services.spawn_blocking(move || {
            let outcome =
                task_services.apply_provider_mutation(ControlMutation::SaveProvider(provider))?;
            Ok::<_, String>((
                outcome.provider_id,
                provider_summaries(&task_services),
                outcome.codex_warning,
            ))
        });
        self.operation = OperationState::Busy(tr!("app.provider.busy.save"));
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(tr!("app.provider.task_failed.save")));
            if let Some(this) = this.upgrade() {
                this.update(cx, |view, cx| {
                    match result {
                        Ok((provider_id, providers, warning)) => {
                            view.providers = providers;
                            view.editing_provider = Some(provider_id.clone());
                            view.selected_page = SettingsPage::Provider(provider_id);
                            view.operation = OperationState::Message {
                                success: warning.is_none(),
                                text: warning.map_or_else(
                                    || {
                                        tr!(
                                            "app.provider.saved",
                                            restart_hint = tr!("app.restart_hint")
                                        )
                                    },
                                    |warning| tr!("app.provider.saved_warning", warning = warning),
                                ),
                            };
                        }
                        Err(error) => {
                            view.operation = OperationState::Message {
                                success: false,
                                text: error,
                            };
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn set_enabled(&mut self, provider_id: ProviderId, enabled: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let services = self.services.clone();
        let task_services = services.clone();
        let receiver = services.spawn_blocking(move || {
            let outcome =
                task_services.apply_provider_mutation(ControlMutation::SetProviderEnabled {
                    provider_id,
                    enabled,
                })?;
            Ok::<_, String>((provider_summaries(&task_services), outcome.codex_warning))
        });
        self.operation = OperationState::Busy(if enabled {
            tr!("app.provider.busy.enable")
        } else {
            tr!("app.provider.busy.disable")
        });
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(tr!("app.provider.task_failed.enabled")));
            if let Some(this) = this.upgrade() {
                this.update(cx, |view, cx| {
                    match result {
                        Ok((providers, warning)) => {
                            view.providers = providers;
                            view.operation = OperationState::Message {
                                success: warning.is_none(),
                                text: warning.map_or_else(
                                    || {
                                        tr!(
                                            "app.provider.state_saved",
                                            restart_hint = tr!("app.restart_hint")
                                        )
                                    },
                                    |warning| tr!("app.provider.state_warning", warning = warning),
                                ),
                            };
                        }
                        Err(error) => {
                            view.operation = OperationState::Message {
                                success: false,
                                text: error,
                            };
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn set_codex_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        self.tray.set_codex_operation_pending(enabled);
        let services = self.services.clone();
        let task_services = services.clone();
        let receiver =
            services.spawn(async move { task_services.set_codex_integration(enabled).await });
        self.operation = OperationState::Busy(if enabled {
            tr!("app.codex.enabling")
        } else {
            tr!("app.codex.disabling")
        });
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(tr!("app.codex.task_failed")));
            if let Some(this) = this.upgrade() {
                this.update(cx, |view, cx| {
                    match result {
                        Ok(status) => {
                            view.apply_codex_status(&status);
                            view.operation = OperationState::Message {
                                success: true,
                                text: if view.codex_integration.enabled {
                                    tr!("app.codex.enabled")
                                } else {
                                    tr!("app.codex.restored")
                                },
                            };
                        }
                        Err(error) => {
                            view.tray.set_codex_enabled(view.codex_integration.enabled);
                            view.operation = OperationState::Message {
                                success: false,
                                text: error,
                            };
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn set_launch_at_login_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        match set_launch_at_login(enabled) {
            Ok(status) => {
                self.launch_at_login = status;
                let requires_approval = status == LaunchAtLoginStatus::RequiresApproval;
                self.operation = OperationState::Message {
                    success: !requires_approval,
                    text: if requires_approval {
                        tr!("app.global.launch_requires_approval")
                    } else if enabled {
                        tr!("app.global.launch_enabled")
                    } else {
                        tr!("app.global.launch_disabled")
                    },
                };
            }
            Err(error) => {
                self.launch_at_login = launch_at_login_status();
                self.operation = OperationState::Message {
                    success: false,
                    text: tr!("app.global.launch_failed", error = error),
                };
            }
        }
        cx.notify();
    }

    fn select_global(&mut self, cx: &mut Context<Self>) {
        self.selected_page = SettingsPage::Global;
        self.operation = OperationState::Idle;
        cx.notify();
    }

    fn select_new_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_editor(window, cx);
        self.selected_page = SettingsPage::NewProvider;
    }

    fn confirm_remove_selected_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let SettingsPage::Provider(provider_id) = self.selected_page.clone() else {
            return;
        };
        let provider_name = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .map_or_else(|| provider_id.to_string(), |provider| provider.name.clone());
        let receiver = window.prompt(
            PromptLevel::Warning,
            &tr!("app.provider.remove_title", name = provider_name),
            Some(&tr!("app.provider.remove_description")),
            &[
                PromptButton::cancel(tr!("app.common.cancel")),
                PromptButton::ok(tr!("app.provider.remove_button")),
            ],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let answer = receiver.await.unwrap_or(0);
            if answer == 1
                && let Some(this) = this.upgrade()
            {
                this.update(cx, |view, cx| view.remove_provider(provider_id, cx));
            }
        })
        .detach();
    }

    fn remove_provider(&mut self, provider_id: ProviderId, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let services = self.services.clone();
        let task_services = services.clone();
        let receiver = services.spawn_blocking(move || {
            let outcome = task_services
                .apply_provider_mutation(ControlMutation::RemoveProvider(provider_id))?;
            Ok::<_, String>((provider_summaries(&task_services), outcome.codex_warning))
        });
        self.operation = OperationState::Busy(tr!("app.provider.busy.remove"));
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(tr!("app.provider.task_failed.remove")));
            if let Some(this) = this.upgrade() {
                this.update(cx, |view, cx| {
                    match result {
                        Ok((providers, warning)) => {
                            view.providers = providers;
                            view.selected_page = SettingsPage::Global;
                            view.editing_provider = None;
                            view.preview = None;
                            view.reviewing_model = None;
                            view.operation = OperationState::Message {
                                success: warning.is_none(),
                                text: warning.map_or_else(
                                    || {
                                        tr!(
                                            "app.provider.removed",
                                            restart_hint = tr!("app.restart_hint")
                                        )
                                    },
                                    |warning| {
                                        tr!("app.provider.removed_warning", warning = warning)
                                    },
                                ),
                            };
                        }
                        Err(error) => {
                            view.operation = OperationState::Message {
                                success: false,
                                text: error,
                            };
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn load_provider(
        &mut self,
        provider_id: &ProviderId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let provider = self.services.control.lock().ok().and_then(|control| {
            control
                .providers()
                .providers
                .iter()
                .find(|provider| &provider.id == provider_id)
                .cloned()
        });
        let Some(provider) = provider else {
            self.operation = OperationState::Message {
                success: false,
                text: tr!("app.provider.disappeared"),
            };
            cx.notify();
            return;
        };
        let template = if provider.id.to_string() == "deepseek"
            || provider.endpoints.http.contains("api.deepseek.com")
        {
            ProviderTemplate::DeepSeek
        } else {
            ProviderTemplate::Custom
        };
        let provider_template = provider_template_label(template);
        self.provider_template = template;
        self.selected_page = SettingsPage::Provider(provider.id.clone());
        self.editing_provider = Some(provider.id.clone());
        self.provider_name
            .update(cx, |input, cx| input.set_value(provider.name, window, cx));
        self.http_url.update(cx, |input, cx| {
            input.set_value(provider.endpoints.http, window, cx);
        });
        self.websocket_url.update(cx, |input, cx| {
            input.set_value(provider.endpoints.websocket.unwrap_or_default(), window, cx);
        });
        let api_key = match &provider.auth {
            AuthConfig::Bearer { api_key } => api_key.clone(),
        };
        self.api_key.update(cx, |input, cx| {
            input.set_value(api_key, window, cx);
            input.set_masked(true, window, cx);
        });
        self.api_key_visibility = ApiKeyVisibility::Hidden;
        self.websocket_enabled = provider.transports.websocket;
        self.protocol = provider.protocol;
        self.provider_select.update(cx, |select, cx| {
            select.set_selected_value(&provider_template, window, cx);
        });
        let protocol = match provider.protocol {
            ProtocolId::OpenaiResponses => RESPONSES_PROTOCOL,
            ProtocolId::OpenaiChatCompletions => CHAT_COMPLETIONS_PROTOCOL,
            ProtocolId::AnthropicMessages => ANTHROPIC_MESSAGES_PROTOCOL,
        };
        self.protocol_select.update(cx, |select, cx| {
            select.set_selected_value(&protocol, window, cx);
        });
        self.preview = None;
        self.reviewing_model = None;
        self.model_search
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.operation = OperationState::Idle;
        cx.notify();
    }

    fn reset_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [
            &self.provider_name,
            &self.http_url,
            &self.websocket_url,
            &self.api_key,
            &self.model_search,
            &self.model_display_name,
            &self.model_context_window,
            &self.model_reasoning_levels,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.api_key
            .update(cx, |input, cx| input.set_masked(true, window, cx));
        self.api_key_visibility = ApiKeyVisibility::Hidden;
        self.websocket_enabled = false;
        self.provider_select.update(cx, |select, cx| {
            select.set_selected_value(&DEEPSEEK_PROVIDER.to_owned(), window, cx);
        });
        self.protocol_select.update(cx, |select, cx| {
            select.set_selected_value(&CHAT_COMPLETIONS_PROTOCOL, window, cx);
        });
        self.protocol = ProtocolId::OpenaiChatCompletions;
        self.provider_template = ProviderTemplate::DeepSeek;
        self.editing_provider = None;
        self.preview = None;
        self.reviewing_model = None;
        self.operation = OperationState::Idle;
        self.apply_deepseek_template(window, cx);
        cx.notify();
    }

    fn load_model_review(
        &mut self,
        model_id: &ModelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.preview.as_ref().and_then(|(_, preview)| {
            preview
                .cache
                .models
                .iter()
                .find(|model| &model.upstream_model_id == model_id)
                .cloned()
        });
        let Some(model) = model else {
            self.operation = OperationState::Message {
                success: false,
                text: tr!("app.model.missing"),
            };
            cx.notify();
            return;
        };
        self.model_display_name.update(cx, |input, cx| {
            input.set_value(model.display_name, window, cx);
        });
        self.model_context_window.update(cx, |input, cx| {
            input.set_value(
                model
                    .context_window
                    .map_or_else(String::new, |value| value.to_string()),
                window,
                cx,
            );
        });
        self.model_reasoning_levels.update(cx, |input, cx| {
            input.set_value(model.supported_reasoning_levels.join(", "), window, cx);
        });
        self.model_parallel_tools = model.supports_parallel_tool_calls.unwrap_or(false);
        self.model_search_tool = model.supports_search_tool.unwrap_or(false);
        self.reviewing_model = Some(model.upstream_model_id);
        self.operation = OperationState::Idle;
        cx.notify();
    }

    fn ensure_model_draft(&mut self, cx: &App) -> bool {
        if self.preview.is_some() {
            return true;
        }
        let Some(provider_id) = self.editing_provider.as_ref() else {
            return false;
        };
        let Some(cache) = self
            .services
            .control
            .lock()
            .ok()
            .and_then(|control| control.cache().providers.get(provider_id).cloned())
        else {
            return false;
        };
        let Ok(provider) = self.draft_provider(cx) else {
            return false;
        };
        let needs_review = cache
            .models
            .iter()
            .filter(|model| model.publication_status == ModelPublicationStatus::NeedsReview)
            .map(|model| model.upstream_model_id.clone())
            .collect();
        self.preview = Some((
            provider,
            RefreshPreview {
                cache,
                added: Vec::new(),
                removed: Vec::new(),
                needs_review,
            },
        ));
        true
    }

    fn apply_model_settings(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(model_id) = self.reviewing_model.clone() else {
            return false;
        };
        let context_window_text = self.model_context_window.read(cx).value();
        let context_window = if context_window_text.trim().is_empty() {
            None
        } else {
            match context_window_text.trim().parse::<u64>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    self.operation = OperationState::Message {
                        success: false,
                        text: tr!("app.model.invalid_context"),
                    };
                    cx.notify();
                    return false;
                }
            }
        };
        let settings = ModelCapabilitySettings {
            display_name: self.model_display_name.read(cx).value().to_string(),
            context_window,
            supported_reasoning_levels: self
                .model_reasoning_levels
                .read(cx)
                .value()
                .split(',')
                .map(str::to_owned)
                .collect(),
            supports_parallel_tool_calls: self.model_parallel_tools,
            supports_search_tool: self.model_search_tool,
        };
        let Some((_, preview)) = self.preview.as_mut() else {
            return false;
        };
        match update_model_capabilities(preview, &model_id, settings) {
            Ok(()) => {
                self.reviewing_model = None;
                self.operation = OperationState::Message {
                    success: true,
                    text: tr!("app.model.updated", model = model_id),
                };
                cx.notify();
                true
            }
            Err(error) => {
                self.operation = OperationState::Message {
                    success: false,
                    text: error.to_string(),
                };
                cx.notify();
                false
            }
        }
    }

    fn open_model_settings(
        &mut self,
        model_id: &ModelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_model_draft(cx) {
            self.operation = OperationState::Message {
                success: false,
                text: tr!("app.model.none_configurable"),
            };
            cx.notify();
            return;
        }
        self.load_model_review(model_id, window, cx);
        let this = cx.entity().downgrade();
        let display_name = self.model_display_name.clone();
        let context_window = self.model_context_window.clone();
        let reasoning_levels = self.model_reasoning_levels.clone();
        let parallel_tools = Rc::new(Cell::new(self.model_parallel_tools));
        let search_tool = Rc::new(Cell::new(self.model_search_tool));
        window.open_dialog(cx, move |dialog, _, _| {
            let parallel_view = this.clone();
            let search_view = this.clone();
            let save_view = this.clone();
            let parallel_tools_state = Rc::clone(&parallel_tools);
            let search_tool_state = Rc::clone(&search_tool);
            dialog
                .title(
                    div()
                        .text_size(px(TEXT_SIZE_DIALOG_TITLE))
                        .font_semibold()
                        .child(tr!("app.model.settings")),
                )
                .w(px(520.0))
                .p(px(24.0))
                .footer(model_settings_footer())
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .pt_2()
                        .gap_4()
                        .text_size(px(TEXT_SIZE_BODY))
                        .child(model_settings_field(
                            tr!("app.model.display_name"),
                            Input::new(&display_name),
                        ))
                        .child(model_settings_field(
                            tr!("app.model.context_window"),
                            Input::new(&context_window),
                        ))
                        .child(model_settings_field(
                            tr!("app.model.reasoning_levels"),
                            Input::new(&reasoning_levels),
                        ))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(tr!("app.model.parallel_tools"))
                                .child(
                                    Switch::new("model-settings-parallel-tools")
                                        .checked(parallel_tools.get())
                                        .on_click(move |checked, _, cx| {
                                            parallel_tools_state.set(*checked);
                                            if let Some(view) = parallel_view.upgrade() {
                                                view.update(cx, |view, cx| {
                                                    view.model_parallel_tools = *checked;
                                                    cx.notify();
                                                });
                                            }
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(tr!("app.model.search_tool"))
                                .child(
                                    Switch::new("model-settings-search-tool")
                                        .checked(search_tool.get())
                                        .on_click(move |checked, _, cx| {
                                            search_tool_state.set(*checked);
                                            if let Some(view) = search_view.upgrade() {
                                                view.update(cx, |view, cx| {
                                                    view.model_search_tool = *checked;
                                                    cx.notify();
                                                });
                                            }
                                        }),
                                ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    save_view
                        .upgrade()
                        .is_some_and(|view| view.update(cx, SettingsView::apply_model_settings))
                })
        });
    }

    fn set_preview_model_enabled(
        &mut self,
        model_id: &ModelId,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_model_draft(cx) {
            self.operation = OperationState::Message {
                success: false,
                text: tr!("app.model.refresh_first"),
            };
            cx.notify();
            return;
        }
        let Some((_, preview)) = self.preview.as_mut() else {
            return;
        };
        match set_model_enabled(preview, model_id, enabled) {
            Ok(()) => {
                self.operation = OperationState::Idle;
            }
            Err(error) => {
                self.operation = OperationState::Message {
                    success: false,
                    text: error.to_string(),
                };
            }
        }
        cx.notify();
    }

    fn save_current(&mut self, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            let provider = match self.draft_provider(cx) {
                Ok(provider) => provider,
                Err(error) => {
                    self.operation = OperationState::Message {
                        success: false,
                        text: error.to_string(),
                    };
                    cx.notify();
                    return;
                }
            };
            if let Some((preview_provider, _)) = self.preview.as_mut() {
                *preview_provider = provider;
            }
            self.commit_preview(cx);
        } else {
            self.save_provider(cx);
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let can_remove = matches!(self.selected_page, SettingsPage::Provider(_));

        div()
            .w(px(248.0))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .flex()
            .flex_col()
            .child(Self::render_sidebar_header(cx))
            .child(self.render_global_navigation(cx))
            .child(
                div()
                    .px_5()
                    .pt_5()
                    .pb_2()
                    .text_size(px(TEXT_SIZE_CAPTION))
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(tr!("app.sidebar.providers")),
            )
            .child(
                div()
                    .id("provider-navigation")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(self.selected_page == SettingsPage::NewProvider, |this| {
                        this.child(Self::render_new_provider_placeholder(cx))
                    })
                    .when(self.providers.is_empty(), |this| {
                        if self.selected_page == SettingsPage::NewProvider {
                            return this;
                        }
                        this.child(
                            div()
                                .px_3()
                                .py_4()
                                .text_size(px(TEXT_SIZE_BODY))
                                .text_color(cx.theme().muted_foreground)
                                .child(tr!("app.sidebar.no_providers")),
                        )
                    })
                    .children(self.providers.clone().into_iter().enumerate().map(
                        |(index, provider)| {
                            self.render_provider_navigation(index, provider, busy, cx)
                                .into_any_element()
                        },
                    )),
            )
            .child(
                div()
                    .h(px(52.0))
                    .flex_none()
                    .px_3()
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("add-provider")
                            .icon(IconName::Plus)
                            .tooltip(tr!("app.sidebar.add_provider"))
                            .ghost()
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.select_new_provider(window, cx);
                            })),
                    )
                    .child(
                        Button::new("remove-provider")
                            .icon(IconName::Minus)
                            .tooltip(tr!("app.sidebar.remove_provider"))
                            .ghost()
                            .small()
                            .disabled(busy || !can_remove)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.confirm_remove_selected_provider(window, cx);
                            })),
                    ),
            )
    }

    fn render_new_provider_placeholder(cx: &App) -> impl IntoElement {
        div()
            .w_full()
            .h(px(54.0))
            .rounded_lg()
            .bg(cx.theme().muted.opacity(0.55))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .child(
                div()
                    .flex_none()
                    .size(px(7.0))
                    .rounded_full()
                    .bg(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .gap_1()
                    .child(
                        div()
                            .h(px(18.0))
                            .line_height(px(18.0))
                            .text_size(px(TEXT_SIZE_BODY))
                            .font_medium()
                            .child(tr!("app.sidebar.new_provider")),
                    )
                    .child(
                        div()
                            .h(px(16.0))
                            .line_height(px(16.0))
                            .text_size(px(TEXT_SIZE_CAPTION))
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("app.sidebar.unsaved")),
                    ),
            )
    }

    fn render_sidebar_header(cx: &App) -> impl IntoElement {
        div()
            .h(px(76.0))
            .px_5()
            .flex()
            .items_center()
            .gap_3()
            .child(
                img(if cx.theme().is_dark() {
                    SETTINGS_APP_ICON_DARK_PATH
                } else {
                    SETTINGS_APP_ICON_LIGHT_PATH
                })
                .size(px(44.0)),
            )
            .child(
                div().flex().child(
                    div()
                        .text_size(px(TEXT_SIZE_BRAND_TITLE))
                        .font_semibold()
                        .child("ProviderX"),
                ),
            )
    }

    fn render_global_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().px_3().child(
            Button::new("global-settings-nav")
                .selected(self.selected_page == SettingsPage::Global)
                .ghost()
                .w_full()
                .h(px(40.0))
                .px_0()
                .rounded_lg()
                .disabled(self.is_busy())
                .child(
                    div()
                        .w(px(224.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(TEXT_SIZE_BODY))
                        .font_medium()
                        .child(tr!("app.sidebar.global_settings")),
                )
                .on_click(cx.listener(|view, _, _, cx| view.select_global(cx))),
        )
    }

    fn render_provider_navigation(
        &self,
        index: usize,
        provider: ProviderSummary,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        let edit_provider_id = provider.id.clone();
        let selected = matches!(
            &self.selected_page,
            SettingsPage::Provider(selected) if selected == &provider.id
        );
        let status_color = if provider.enabled {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        Button::new(("provider-nav", index))
            .selected(selected)
            .ghost()
            .w_full()
            .h(px(54.0))
            .px_0()
            .disabled(busy)
            .child(
                div()
                    .w(px(224.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_start()
                    .gap_2()
                    .px_3()
                    .child(
                        div()
                            .flex_none()
                            .size(px(7.0))
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .items_start()
                            .justify_center()
                            .gap_1()
                            .child(
                                div()
                                    .w_full()
                                    .h(px(18.0))
                                    .line_height(px(18.0))
                                    .text_size(px(TEXT_SIZE_BODY))
                                    .font_medium()
                                    .truncate()
                                    .child(provider.name),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .h(px(16.0))
                                    .line_height(px(16.0))
                                    .text_size(px(TEXT_SIZE_CAPTION))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(tr!(
                                        "app.common.model_count",
                                        count = provider.ready_models + provider.review_models
                                    )),
                            ),
                    ),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.load_provider(&edit_provider_id, window, cx);
            }))
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        div()
            .grid()
            .grid_cols(2)
            .gap_x_5()
            .gap_y_4()
            .child(select_field(
                tr!("app.provider.field.template"),
                Select::new(&self.provider_select)
                    .w_full()
                    .text_size(px(TEXT_SIZE_BODY))
                    .disabled(busy || self.editing_provider.is_some()),
            ))
            .child(field(
                tr!("app.provider.field.name"),
                Input::new(&self.provider_name),
            ))
            .child(field("HTTP API URL", Input::new(&self.http_url)))
            .child(select_field(
                tr!("app.provider.field.protocol"),
                Select::new(&self.protocol_select)
                    .w_full()
                    .text_size(px(TEXT_SIZE_BODY))
                    .disabled(busy),
            ))
            .child(field(
                tr!("app.provider.field.websocket_url"),
                Input::new(&self.websocket_url),
            ))
            .child(field(
                "API Key",
                Input::new(&self.api_key).suffix(
                    Button::new("toggle-api-key-visibility")
                        .icon(if self.api_key_visibility == ApiKeyVisibility::Visible {
                            IconName::EyeOff
                        } else {
                            IconName::Eye
                        })
                        .xsmall()
                        .ghost()
                        .tab_stop(false)
                        .tooltip(if self.api_key_visibility == ApiKeyVisibility::Visible {
                            tr!("app.provider.api_key.hide")
                        } else {
                            tr!("app.provider.api_key.show")
                        })
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.api_key_visibility = match view.api_key_visibility {
                                ApiKeyVisibility::Hidden => ApiKeyVisibility::Visible,
                                ApiKeyVisibility::Visible => ApiKeyVisibility::Hidden,
                            };
                            let masked = view.api_key_visibility == ApiKeyVisibility::Hidden;
                            view.api_key.update(cx, |input, cx| {
                                input.set_masked(masked, window, cx);
                            });
                            cx.notify();
                        })),
                ),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .h(px(18.0))
                            .line_height(px(18.0))
                            .text_size(px(TEXT_SIZE_BODY))
                            .font_medium()
                            .child(tr!("app.provider.field.native_websocket")),
                    )
                    .child(
                        Switch::new("draft-websocket-enabled")
                            .checked(self.websocket_enabled)
                            .disabled(busy || self.protocol != ProtocolId::OpenaiResponses)
                            .on_click(cx.listener(|view, checked, _, cx| {
                                view.websocket_enabled = *checked;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn visible_models(&self, cx: &App) -> (usize, Vec<ProviderModelSpec>) {
        let models = if let Some((_, preview)) = &self.preview {
            preview.cache.models.clone()
        } else if let Some(provider_id) = self.editing_provider.as_ref() {
            self.services
                .control
                .lock()
                .ok()
                .and_then(|control| control.cache().providers.get(provider_id).cloned())
                .map_or_else(Vec::new, |cache| cache.models)
        } else {
            Vec::new()
        };
        let total = models.len();
        let query = self.model_search.read(cx).value().trim().to_owned();
        if query.is_empty() {
            return (total, models);
        }
        (
            total,
            models
                .into_iter()
                .filter(|model| model_matches_search(model, &query))
                .collect(),
        )
    }

    fn render_model_row(
        &self,
        index: usize,
        model: ProviderModelSpec,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let settings_model_id = model.upstream_model_id.clone();
        let switch_model_id = model.upstream_model_id.clone();
        let enabled = model.publication_status == ModelPublicationStatus::Ready;
        div()
            .min_h(px(48.0))
            .px_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(TEXT_SIZE_BODY))
                            .truncate()
                            .child(model.display_name),
                    )
                    .child(
                        div()
                            .text_size(px(TEXT_SIZE_CAPTION))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(model.catalog_model_id.to_string()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_none()
                    .child(
                        Button::new(("model-settings", index))
                            .icon(IconName::Settings2)
                            .tooltip(tr!("app.model.settings_tooltip"))
                            .ghost()
                            .small()
                            .disabled(self.is_busy())
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.open_model_settings(&settings_model_id, window, cx);
                            })),
                    )
                    .child(
                        Switch::new(("model-enabled", index))
                            .checked(enabled)
                            .disabled(self.is_busy())
                            .on_click(cx.listener(move |view, checked, _, cx| {
                                view.set_preview_model_enabled(&switch_model_id, *checked, cx);
                            })),
                    ),
            )
            .when(index == 0, gpui::Styled::rounded_t_lg)
            .into_any_element()
    }

    fn render_model_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (total_models, models) = self.visible_models(cx);
        let searching = !self.model_search.read(cx).value().trim().is_empty();
        let model_count = if searching {
            tr!(
                "app.model.list_filtered",
                visible = models.len(),
                total = total_models
            )
        } else {
            tr!("app.model.list_total", total = total_models)
        };
        let rows = models
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, model)| self.render_model_row(index, model, cx))
            .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .px_1()
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(TEXT_SIZE_BODY))
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child(model_count),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Input::new(&self.model_search)
                                    .prefix(Icon::new(IconName::Search).small())
                                    .cleanable(true)
                                    .small()
                                    .text_size(px(TEXT_SIZE_CAPTION))
                                    .font_normal()
                                    .w(px(240.0)),
                            )
                            .child(
                                Button::new("refresh-model-list")
                                    .icon(AppIcon::RefreshModels)
                                    .tooltip(tr!("app.model.refresh_tooltip"))
                                    .ghost()
                                    .small()
                                    .disabled(self.is_busy())
                                    .on_click(cx.listener(|view, _, _, cx| view.start_refresh(cx))),
                            ),
                    ),
            )
            .child(
                div()
                    .id("provider-model-list")
                    .h(px(128.0))
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .overflow_y_scrollbar()
                    .when(models.is_empty(), |this| {
                        this.child(
                            div()
                                .h_full()
                                .w_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_center()
                                .text_size(px(TEXT_SIZE_BODY))
                                .text_color(cx.theme().muted_foreground)
                                .child(if searching && total_models > 0 {
                                    tr!("app.model.no_match")
                                } else {
                                    tr!("app.model.empty")
                                }),
                        )
                    })
                    .children(rows),
            )
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let feedback = match &self.operation {
            OperationState::Busy(text) => Some(
                div()
                    .text_size(px(TEXT_SIZE_CAPTION))
                    .text_color(cx.theme().muted_foreground)
                    .child(text.clone())
                    .into_any_element(),
            ),
            OperationState::Message {
                success: false,
                text,
            } => Some(
                div()
                    .text_size(px(TEXT_SIZE_CAPTION))
                    .text_color(cx.theme().danger)
                    .child(text.clone())
                    .into_any_element(),
            ),
            OperationState::Idle | OperationState::Message { success: true, .. } => None,
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(div().flex_1().min_w_0().children(feedback))
            .child(
                Button::new("save-provider")
                    .label(tr!("app.common.save"))
                    .text_size(px(TEXT_SIZE_BODY))
                    .primary()
                    .disabled(busy)
                    .on_click(cx.listener(|view, _, _, cx| view.save_current(cx))),
            )
    }

    fn render_global_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(TEXT_SIZE_PAGE_TITLE))
                            .font_semibold()
                            .child(tr!("app.sidebar.global_settings")),
                    )
                    .child(
                        div()
                            .text_size(px(TEXT_SIZE_BODY))
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("app.global.subtitle")),
                    ),
            )
            .child(self.render_runtime_settings(cx))
            .child(Self::render_about_settings(cx))
    }

    fn render_runtime_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.is_busy();
        let integration_hint = match &self.operation {
            OperationState::Busy(text)
                if text == &tr!("app.codex.enabling") || text == &tr!("app.codex.disabling") =>
            {
                text.clone()
            }
            _ if self.codex_integration.enabled => tr!("app.global.integration_enabled_hint"),
            _ => tr!("app.global.integration_disabled_hint"),
        };
        let launch_enabled = matches!(
            self.launch_at_login,
            LaunchAtLoginStatus::Enabled | LaunchAtLoginStatus::RequiresApproval
        );
        let launch_hint = match self.launch_at_login {
            LaunchAtLoginStatus::RequiresApproval => tr!("app.global.launch_waiting"),
            _ => tr!("app.global.launch_description"),
        };
        settings_group(
            tr!("app.global.runtime_group"),
            div()
                .child(settings_row(
                    tr!("app.global.launch_at_login"),
                    launch_hint,
                    Switch::new("launch-at-login")
                        .checked(launch_enabled)
                        .disabled(busy)
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.set_launch_at_login_enabled(*checked, cx);
                        })),
                    cx,
                ))
                .child(settings_separator(cx))
                .child(settings_select_row(
                    tr!("app.language.title"),
                    tr!("app.language.description"),
                    Select::new(&self.language_select)
                        .w_full()
                        .text_size(px(TEXT_SIZE_BODY))
                        .disabled(busy),
                    cx,
                ))
                .child(settings_separator(cx))
                .child(settings_row(
                    tr!("app.global.codex_integration"),
                    integration_hint,
                    Switch::new("codex-integration")
                        .checked(self.codex_integration.enabled)
                        .disabled(busy)
                        .on_click(cx.listener(|view, checked, _, cx| {
                            view.set_codex_enabled(*checked, cx);
                        })),
                    cx,
                )),
            cx,
        )
    }

    fn render_about_settings(cx: &mut Context<Self>) -> impl IntoElement {
        settings_group(
            tr!("app.global.about"),
            div()
                .child(info_row(
                    tr!("app.global.version"),
                    format!("v{APP_VERSION}"),
                    cx,
                ))
                .child(settings_separator(cx))
                .child(info_row(
                    tr!("app.global.description_title"),
                    tr!("app.global.description"),
                    cx,
                ))
                .child(settings_separator(cx))
                .child(
                    div()
                        .px_4()
                        .py_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child("GitHub")
                        .child(
                            Link::new("github-repository")
                                .href(GITHUB_URL)
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(px(TEXT_SIZE_BODY))
                                .child(Icon::new(IconName::Github).small())
                                .child("brookqin/provider-x"),
                        ),
                ),
            cx,
        )
    }

    fn render_provider_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_new = self.selected_page == SettingsPage::NewProvider;
        let selected_summary = self.editing_provider.as_ref().and_then(|provider_id| {
            self.providers
                .iter()
                .find(|provider| &provider.id == provider_id)
        });
        let title = if is_new {
            tr!("app.provider.add_title")
        } else {
            selected_summary.map_or_else(
                || tr!("app.provider.settings_title"),
                |item| item.name.clone(),
            )
        };
        let subtitle = if is_new {
            tr!("app.provider.add_subtitle")
        } else {
            selected_summary.map_or_else(
                || tr!("app.provider.settings_subtitle"),
                |item| {
                    format!(
                        "{} · {}",
                        item.id,
                        tr!(
                            "app.common.model_count",
                            count = item.ready_models + item.review_models
                        )
                    )
                },
            )
        };
        let provider_switch = selected_summary.map(|provider| {
            let provider_id = provider.id.clone();
            let can_enable = provider.ready_models > 0;
            Switch::new("selected-provider-enabled")
                .checked(provider.enabled)
                .disabled(self.is_busy() || (!provider.enabled && !can_enable))
                .on_click(cx.listener(move |view, checked, _, cx| {
                    view.set_enabled(provider_id.clone(), *checked, cx);
                }))
        });

        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .h(px(54.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(TEXT_SIZE_PAGE_TITLE))
                                    .font_semibold()
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(px(TEXT_SIZE_BODY))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(subtitle),
                            ),
                    )
                    .children(provider_switch),
            )
            .child(
                div()
                    .p_5()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .child(self.render_editor(cx)),
            )
            .child(self.render_model_list(cx))
            .child(self.render_actions(cx))
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = match &self.selected_page {
            SettingsPage::Global => self.render_global_settings(cx).into_any_element(),
            SettingsPage::Provider(_) | SettingsPage::NewProvider => {
                self.render_provider_detail(cx).into_any_element()
            }
        };
        div()
            .relative()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(px(TEXT_SIZE_BODY))
            .flex()
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("settings-details")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .child(div().w_full().p_7().child(details)),
                    )
                    .child(
                        div()
                            .h(px(52.0))
                            .flex_none()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(div()),
                    ),
            )
            .when_some(Root::render_dialog_layer(window, cx), |this, layer| {
                this.child(layer)
            })
    }
}

fn field(label: impl Into<SharedString>, input: Input) -> impl IntoElement {
    let label = label.into();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .h(px(18.0))
                .line_height(px(18.0))
                .text_size(px(TEXT_SIZE_BODY))
                .font_medium()
                .truncate()
                .child(label),
        )
        .child(input.text_size(px(TEXT_SIZE_BODY)))
}

fn model_settings_field(label: impl Into<SharedString>, input: Input) -> impl IntoElement {
    let label = label.into();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .text_size(px(TEXT_SIZE_BODY))
        .child(
            div()
                .h(px(18.0))
                .line_height(px(18.0))
                .font_medium()
                .truncate()
                .child(label),
        )
        .child(input.text_size(px(TEXT_SIZE_BODY)))
}

fn select_field(label: impl Into<SharedString>, select: impl IntoElement) -> impl IntoElement {
    let label = label.into();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .h(px(18.0))
                .line_height(px(18.0))
                .text_size(px(TEXT_SIZE_BODY))
                .font_medium()
                .child(label),
        )
        .child(select)
}

fn model_settings_footer() -> impl IntoElement {
    DialogFooter::new()
        .w_full()
        .justify_end()
        .child(
            div().w(px(72.0)).child(
                DialogClose::new().child(
                    Button::new("cancel-model-settings")
                        .w_full()
                        .small()
                        .text_size(px(TEXT_SIZE_BODY))
                        .label(tr!("app.common.cancel"))
                        .outline(),
                ),
            ),
        )
        .child(
            div().w(px(72.0)).child(
                DialogAction::new().child(
                    Button::new("save-model-settings")
                        .w_full()
                        .small()
                        .text_size(px(TEXT_SIZE_BODY))
                        .label(tr!("app.common.save"))
                        .primary(),
                ),
            ),
        )
}

fn settings_group(
    title: impl Into<SharedString>,
    content: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    let title = title.into();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .px_1()
                .text_size(px(TEXT_SIZE_BODY))
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
        .child(
            div()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().border)
                .overflow_hidden()
                .child(content),
        )
}

fn settings_row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    let title = title.into();
    let description = description.into();
    div()
        .px_4()
        .py_3()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div().flex().flex_col().gap_1().child(title).child(
                div()
                    .text_size(px(TEXT_SIZE_CAPTION))
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            ),
        )
        .child(control)
}

fn settings_select_row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    select: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    let title = title.into();
    let description = description.into();
    div()
        .px_4()
        .py_3()
        .grid()
        .grid_cols(2)
        .items_center()
        .gap_6()
        .child(
            div().flex().flex_col().gap_1().child(title).child(
                div()
                    .text_size(px(TEXT_SIZE_CAPTION))
                    .text_color(cx.theme().muted_foreground)
                    .child(description),
            ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_end()
                .child(div().w(px(180.0)).child(select)),
        )
}

fn info_row(
    title: impl Into<SharedString>,
    value: impl Into<SharedString>,
    cx: &App,
) -> impl IntoElement {
    let title = title.into();
    let value = value.into();
    div()
        .px_4()
        .py_3()
        .flex()
        .items_start()
        .justify_between()
        .gap_6()
        .child(title)
        .child(
            div()
                .max_w(px(420.0))
                .text_right()
                .text_size(px(TEXT_SIZE_BODY))
                .text_color(cx.theme().muted_foreground)
                .child(value),
        )
}

fn settings_separator(cx: &App) -> impl IntoElement {
    div().mx_4().h(px(1.0)).bg(cx.theme().border)
}

fn next_available_provider_id<'a>(
    base: &str,
    existing: impl IntoIterator<Item = &'a str>,
) -> String {
    let existing = existing.into_iter().collect::<Vec<_>>();
    if !existing.contains(&base) {
        return base.to_owned();
    }
    let mut suffix = 2_u64;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !existing.contains(&candidate.as_str()) {
            return candidate;
        }
        suffix = suffix
            .checked_add(1)
            .expect("Provider namespace suffix space is exhausted");
    }
}

fn provider_namespace_from_name(name: &str) -> anyhow::Result<String> {
    let mut namespace = String::with_capacity(name.len());
    let mut pending_separator = false;

    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !namespace.is_empty() {
                namespace.push('-');
            }
            namespace.push(character.to_ascii_lowercase());
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }

    anyhow::ensure!(
        !namespace.is_empty(),
        tr!("app.provider.namespace_required")
    );
    Ok(namespace)
}

fn model_matches_search(model: &ProviderModelSpec, query: &str) -> bool {
    let haystack = format!(
        "{} {}",
        model.display_name,
        model.upstream_model_id.as_str()
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|keyword| haystack.contains(keyword))
}

fn new_model_search(window: &mut Window, cx: &mut Context<SettingsView>) -> Entity<InputState> {
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(tr!("app.model.search_placeholder")));
    cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            cx.notify();
        }
    })
    .detach();
    input
}

fn new_provider_select(
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> Entity<SelectState<Vec<String>>> {
    cx.new(|cx| {
        SelectState::new(
            provider_template_options(),
            Some(IndexPath::default()),
            window,
            cx,
        )
    })
}

fn new_language_select(
    locale: UiLocale,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> Entity<SelectState<Vec<&'static str>>> {
    let selected_index = usize::from(locale != UiLocale::SimplifiedChinese);
    cx.new(|cx| {
        SelectState::new(
            vec![SIMPLIFIED_CHINESE_LABEL, ENGLISH_LABEL],
            Some(IndexPath::new(selected_index)),
            window,
            cx,
        )
    })
}

fn new_protocol_select(
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> Entity<SelectState<Vec<&'static str>>> {
    cx.new(|cx| {
        SelectState::new(
            vec![
                RESPONSES_PROTOCOL,
                CHAT_COMPLETIONS_PROTOCOL,
                ANTHROPIC_MESSAGES_PROTOCOL,
            ],
            Some(IndexPath::default()),
            window,
            cx,
        )
    })
}

fn provider_template_options() -> Vec<String> {
    vec![DEEPSEEK_PROVIDER.to_owned(), tr!("app.common.custom")]
}

fn provider_template_label(template: ProviderTemplate) -> String {
    match template {
        ProviderTemplate::DeepSeek => DEEPSEEK_PROVIDER.to_owned(),
        ProviderTemplate::Custom => tr!("app.common.custom"),
    }
}

fn provider_summaries(services: &AppServices) -> Vec<ProviderSummary> {
    services
        .control
        .lock()
        .map_or_else(|_| Vec::new(), |control| summaries_from_control(&control))
}

fn summaries_from_control(control: &ControlPlane) -> Vec<ProviderSummary> {
    control
        .providers()
        .providers
        .iter()
        .map(|provider| {
            let (ready_models, review_models) =
                control
                    .cache()
                    .providers
                    .get(&provider.id)
                    .map_or((0, 0), |cache| {
                        cache.models.iter().fold((0, 0), |(ready, review), model| {
                            if model.publication_status == ModelPublicationStatus::Ready {
                                (ready + 1, review)
                            } else {
                                (ready, review + 1)
                            }
                        })
                    });
            ProviderSummary {
                id: provider.id.clone(),
                name: provider.name.clone(),
                enabled: provider.enabled,
                ready_models,
                review_models,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use gpui::AssetSource;
    use gpui_component::{IconName, IconNamed};

    use super::{
        AppAssets, EMBEDDED_ASSETS, REFRESH_MODELS_ICON_PATH, SETTINGS_APP_ICON_DARK_PATH,
        SETTINGS_APP_ICON_LIGHT_PATH, next_available_provider_id, provider_namespace_from_name,
        redacted_codex_disable_diagnostic,
    };

    #[test]
    fn codex_disable_diagnostic_does_not_expose_configuration_error_details() {
        let sensitive_error = "TOML parse error near api_key = sensitive-config-sentinel";
        let diagnostic = redacted_codex_disable_diagnostic(sensitive_error);

        assert_eq!(
            diagnostic,
            "failed to disable Codex integration; open settings for details"
        );
        assert!(!diagnostic.contains("sensitive-config-sentinel"));
        assert!(!diagnostic.contains("api_key"));
    }

    #[test]
    fn minimal_asset_bundle_covers_every_app_and_component_icon() {
        let assets = AppAssets;
        let required = [
            IconName::ChevronDown,
            IconName::CircleX,
            IconName::Close,
            IconName::Eye,
            IconName::EyeOff,
            IconName::Github,
            IconName::Inbox,
            IconName::Minus,
            IconName::Plus,
            IconName::Search,
            IconName::Settings2,
        ];
        for icon in required {
            let path = icon.path();
            assert!(
                assets.load(path.as_ref()).unwrap().is_some(),
                "missing bundled icon {path}"
            );
        }
        assert!(assets.load(REFRESH_MODELS_ICON_PATH).unwrap().is_some());
        assert!(assets.load(SETTINGS_APP_ICON_LIGHT_PATH).unwrap().is_some());
        assert!(assets.load(SETTINGS_APP_ICON_DARK_PATH).unwrap().is_some());
        assert_eq!(assets.list("").unwrap().len(), EMBEDDED_ASSETS.len());
        assert!(assets.load("icons/not-bundled.svg").is_err());
    }

    #[test]
    fn new_provider_namespace_is_stable_and_skips_existing_suffixes() {
        assert_eq!(next_available_provider_id("deepseek", []), "deepseek");
        assert_eq!(
            next_available_provider_id("deepseek", ["deepseek", "deepseek-2", "other"]),
            "deepseek-3"
        );
        assert_eq!(
            next_available_provider_id("open-router", ["open-router", "open-router-3"]),
            "open-router-2"
        );
    }

    #[test]
    fn custom_provider_namespace_is_derived_from_its_name() {
        assert_eq!(
            provider_namespace_from_name(" Open Router ").unwrap(),
            "open-router"
        );
        assert_eq!(
            provider_namespace_from_name("Acme_API.v2").unwrap(),
            "acme-api-v2"
        );
        assert_eq!(
            provider_namespace_from_name("深度 DeepSeek 服务").unwrap(),
            "deepseek"
        );
        assert!(provider_namespace_from_name("自定义供应商").is_err());
    }
}
