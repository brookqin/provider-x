use crate::codex_config::{CodexConfigStatus, ReceiptPhase};
use crate::localization::{UiLocale, UiLocaleStore};
use crate::platform::macos::{
    is_accessory_activation_policy, set_dock_visible,
    tray::{MacTrayController, TrayCommand},
};
use crate::runtime::AppServices;
use crate::{control_plane::AppPaths, storage::SingleInstanceGuard};
use gpui_kit::{
    AnyWindowHandle, App, Bounds, Global, TitlebarOptions, WeakEntity, WindowBounds, WindowOptions,
    px, size,
};
use std::{cell::Cell, rc::Rc, time::Duration};
const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(40);
const SETTINGS_RELEASE_GRACE: Duration = Duration::from_secs(5);
const SMOKE_SETTINGS_RELEASE_GRACE: Duration = Duration::from_millis(250);
const SMOKE_RELEASE_OBSERVATION_DELAY: Duration = Duration::from_secs(5);
macro_rules! tr {
    ($key:literal) => {
        rust_i18n::t!($key).to_string()
    };
}

struct TrayControllerGlobal {
    _controller: Rc<MacTrayController>,
}

impl Global for TrayControllerGlobal {}

struct SettingsRegistry {
    view: Option<WeakEntity<gpui_shell::ShellRoot>>,
    components_initialized: bool,
    runtime: Option<Rc<gpui_shell::ShellRuntime>>,
    watcher: Option<gpui_shell::Watcher>,
    release_generation: u64,
    release_pending: bool,
    release_delay: Duration,
    quitting: bool,
}

impl SettingsRegistry {
    fn new(release_delay: Duration) -> Self {
        Self {
            view: None,
            components_initialized: false,
            runtime: None,
            watcher: None,
            release_generation: 0,
            release_pending: false,
            release_delay,
            quitting: false,
        }
    }

    fn cancel_release(&mut self) {
        self.release_generation = self.release_generation.wrapping_add(1);
        self.release_pending = false;
    }

    fn begin_release(&mut self) -> (u64, Duration) {
        self.release_generation = self.release_generation.wrapping_add(1);
        self.release_pending = true;
        (self.release_generation, self.release_delay)
    }

    fn finish_release(&mut self, generation: u64) -> bool {
        if !self.release_pending || self.release_generation != generation {
            return false;
        }
        self.release_pending = false;
        self.view = None;
        self.watcher = None;
        self.runtime = None;
        true
    }
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
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();
    let options = LaunchOptions::from_args()?;
    let paths = AppPaths::for_home(crate::runtime::data_home()?);
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

    let application = gpui_kit::application().with_assets(gpui_shell::AppAssets::new(ui_root()?));
    application.on_reopen(|cx| {
        if cx.has_global::<SettingsRegistry>() && !cx.global::<SettingsRegistry>().quitting {
            let _ = open_or_focus_settings(cx);
        }
    });
    application.run(move |cx| {
        if let Err(error) = launch(cx, options) {
            // AppKit termination may exit without returning from Application::run.
            // Report before quitting, including launches from Finder without a terminal.
            // A native modal runs a nested event loop; release GPUI's App borrow first.
            cx.spawn(async move |cx| {
                crate::platform::macos::show_startup_failure(&error);
                *reported_error.borrow_mut() = Some(error);
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });

    if let Some(error) = startup_error.borrow_mut().take() {
        Err(error)
    } else {
        Ok(())
    }
}

fn launch(cx: &mut App, options: LaunchOptions) -> anyhow::Result<()> {
    let services = if options.smoke_lifecycle
        || (cfg!(debug_assertions) && std::env::var_os("PROVIDER_X_TEST_HOME").is_some())
    {
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
    let upgrade_notice = services.upgrade_notice.clone();
    cx.set_global(services);
    let dock_visible = apply_saved_dock_preference()?;

    let tray = Rc::new(MacTrayController::new(listener_address, codex_enabled)?);
    cx.set_global(TrayControllerGlobal {
        _controller: Rc::clone(&tray),
    });
    let settings_release_delay = if options.smoke_lifecycle {
        SMOKE_SETTINGS_RELEASE_GRACE
    } else {
        SETTINGS_RELEASE_GRACE
    };
    cx.set_global(SettingsRegistry::new(settings_release_delay));
    cx.on_window_closed(|cx, _| {
        let registry = cx.global_mut::<SettingsRegistry>();
        registry.view = None;
        registry.cancel_release();
    })
    .detach();
    println!(
        "PROVIDER_X_SMOKE tray=ready activation_policy={}",
        if dock_visible { "regular" } else { "accessory" }
    );
    println!("PROVIDER_X_SMOKE settings_ui=deferred");

    if options.show_settings && upgrade_notice.is_none() {
        open_or_focus_settings(cx)?;
    }

    show_provider_upgrade_notice(cx, upgrade_notice);

    let tray_task = Rc::clone(&tray);
    cx.spawn(async move |cx| {
        let mut locale = rust_i18n::locale().to_string();
        loop {
            let current = rust_i18n::locale().to_string();
            if locale != current {
                tray_task.refresh_locale();
                locale = current;
            }
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

fn apply_saved_dock_preference() -> anyhow::Result<bool> {
    let visible = crate::ui_preferences::load_dock_visible(
        &AppPaths::for_home(crate::runtime::data_home()?)
            .root
            .join("ui-dock.json"),
    )?;
    set_dock_visible(visible)?;
    anyhow::ensure!(
        is_accessory_activation_policy() != visible,
        "macOS activation policy did not match Dock preference"
    );
    Ok(visible)
}

fn show_provider_upgrade_notice(
    cx: &mut App,
    upgrade_notice: Option<crate::storage::provider_upgrade::UpgradeNotice>,
) {
    if let Some(notice) = upgrade_notice {
        // Native modals must run outside an App borrow because they pump AppKit events.
        cx.spawn(async move |cx| {
            crate::platform::macos::show_upgrade_notice(&notice.backup);
            if notice.acknowledge().is_err() {
                eprintln!("ProviderX: upgrade_notice_acknowledgement_failed");
            }
            cx.update(|cx| {
                if open_or_focus_settings(cx).is_err() {
                    eprintln!("ProviderX: open_settings_failed");
                }
            });
        })
        .detach();
    }
}

fn spawn_smoke_lifecycle(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(600))
            .await;
        cx.update(|cx| {
            if cx.windows().is_empty()
                && let Err(error) = open_or_focus_settings(cx)
            {
                eprintln!("failed to open settings during lifecycle smoke: {error:#}");
                cx.quit();
            }
        });
        smoke_dock_window_visibility(cx).await;
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
        cx.update(|cx| {
            if let Some(handle) = cx.windows().first().copied() {
                schedule_settings_window_release(handle, cx);
                println!("PROVIDER_X_SMOKE lifecycle=window_hidden_pending_release");
            } else {
                eprintln!("settings window was not open during lifecycle smoke");
                cx.quit();
            }
        });
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        cx.update(|cx| {
            if let Err(error) = open_or_focus_settings(cx) {
                eprintln!("failed to reopen retained settings: {error:#}");
                cx.quit();
            } else {
                println!("PROVIDER_X_SMOKE lifecycle=window_reopened_before_release");
            }
        });
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        cx.update(|cx| {
            if let Some(handle) = cx.windows().first().copied() {
                schedule_settings_window_release(handle, cx);
            } else {
                eprintln!("retained settings window disappeared before release smoke");
                cx.quit();
            }
        });
        cx.background_executor()
            .timer(SMOKE_SETTINGS_RELEASE_GRACE + Duration::from_millis(100))
            .await;
        cx.update(|cx| {
            if !cx.windows().is_empty() {
                eprintln!("settings window was not released after the grace period");
                cx.quit();
                return;
            }
            println!("PROVIDER_X_SMOKE lifecycle=window_closed_process_alive");
        });
        cx.background_executor()
            .timer(SMOKE_RELEASE_OBSERVATION_DELAY)
            .await;
        cx.update(|cx| {
            if let Err(error) = open_or_focus_settings(cx) {
                eprintln!("failed to reopen settings after release: {error:#}");
                cx.quit();
            } else {
                println!("PROVIDER_X_SMOKE lifecycle=window_reopened");
            }
        });
    })
    .detach();
}

async fn smoke_dock_window_visibility(cx: &mut gpui_kit::AsyncApp) {
    for visible in [true, false] {
        let changed = cx.update(|_| {
            crate::platform::macos::has_visible_window() && set_dock_visible(visible).is_ok()
        });
        // Observe after AppKit has processed the policy transition, without activating the app.
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        cx.update(|cx| {
            if !changed || !crate::platform::macos::has_visible_window() {
                eprintln!("Dock visibility change hid the settings window");
                cx.quit();
            } else {
                println!("PROVIDER_X_SMOKE dock_visible={visible} settings_window=visible");
            }
        });
    }
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
    _status: &CodexConfigStatus,
    message: &str,
    _success: bool,
) {
    if cx.has_global::<crate::ui_host::UiHost>() {
        cx.global::<crate::ui_host::UiHost>()
            .set_message(message.to_owned());
    }
}

fn redacted_codex_disable_diagnostic(_error: &str) -> &'static str {
    "failed to disable Codex integration; open settings for details"
}

fn sync_settings_codex_error(cx: &mut App, error: String) {
    if cx.has_global::<crate::ui_host::UiHost>() {
        cx.global::<crate::ui_host::UiHost>().set_message(error);
    }
}

fn graceful_quit(cx: &mut App) {
    cx.global_mut::<SettingsRegistry>().quitting = true;
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
    cx.global_mut::<SettingsRegistry>().cancel_release();
    if let Some(handle) = cx.windows().first().copied() {
        cx.activate(true);
        handle.update(cx, |_, window, _| window.activate_window())?;
        return Ok(());
    }

    ensure_settings_ui_initialized(cx)?;
    let runtime = gpui_shell::ShellRuntime::new(cx)?;
    let builder_runtime = Rc::clone(&runtime);
    let root_path = ui_root()?;
    let script_failed = Rc::new(Cell::new(false));
    let check_failed = Rc::clone(&script_failed);
    let bounds = Bounds::centered(None, size(px(1080.0), px(760.0)), cx);
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
        move |window, cx| {
            let window_handle = window.window_handle();
            window.on_window_should_close(cx, move |_, cx| {
                if cx.global::<SettingsRegistry>().quitting {
                    return true;
                }
                schedule_settings_window_release(window_handle, cx);
                false
            });
            let root = if let Ok(root) = builder_runtime.try_load(&root_path, window, cx) {
                println!("PROVIDER_X_SMOKE settings_script=loaded");
                root
            } else {
                check_failed.set(true);
                builder_runtime.load(&root_path, window, cx)
            };
            if cfg!(debug_assertions) && std::env::var_os("PROVIDER_X_UI_WATCH").is_some() {
                match builder_runtime.watch(&root, window, cx) {
                    Ok(watcher) => cx.global_mut::<SettingsRegistry>().watcher = Some(watcher),
                    Err(_) => eprintln!("could not watch settings scripts"),
                }
            }
            let registry = cx.global_mut::<SettingsRegistry>();
            registry.view = Some(root.downgrade());
            registry.runtime = Some(builder_runtime);
            root
        },
    )?;
    anyhow::ensure!(!script_failed.get(), "settings script load failed");
    cx.activate(true);
    println!("PROVIDER_X_SMOKE settings_window=open");
    Ok(())
}

fn ensure_settings_ui_initialized(cx: &mut App) -> anyhow::Result<()> {
    if !cx.global::<SettingsRegistry>().components_initialized {
        gpui_shell::init(cx);
        crate::ui_host::register(cx)?;
        cx.global_mut::<SettingsRegistry>().components_initialized = true;
        println!("PROVIDER_X_SMOKE settings_ui=initialized");
    }
    Ok(())
}

/// Generates the same declarations as the runtime before application resources are signed.
/// # Errors
/// Returns a host-module registration or filesystem error.
pub fn prepare_ui_bundle(root: &std::path::Path) -> anyhow::Result<()> {
    // Build tooling exposes only signatures, without creating services or touching user data.
    let mut module = gpui_shell::HostModule::new("providerx");
    for name in [
        "action", "edit", "open", "secret", "select", "snapshot", "text",
    ] {
        module = module.function(name, |_| {
            Err(gpui_shell::HostError::new("declaration-only host"))
        });
    }
    gpui_shell::export_module(module)?;
    gpui_shell::write_type_declarations_with_components(
        root,
        &gpui_shell::FrozenComponentRegistry::default(),
    )?;
    Ok(())
}

fn ui_root() -> anyhow::Result<std::path::PathBuf> {
    if cfg!(debug_assertions) {
        return Ok(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui"));
    }
    let executable = std::env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| anyhow::anyhow!("application bundle is missing"))?;
    let root = contents.join("Resources/ui");
    anyhow::ensure!(
        root.join("main.js").is_file(),
        "bundled settings UI is missing"
    );
    Ok(root)
}

fn schedule_settings_window_release(handle: AnyWindowHandle, cx: &mut App) {
    let (generation, delay) = cx.global_mut::<SettingsRegistry>().begin_release();
    cx.hide();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(delay).await;
        cx.update(|cx| finish_settings_window_release(handle, generation, cx));
    })
    .detach();
}

fn finish_settings_window_release(handle: AnyWindowHandle, generation: u64, cx: &mut App) {
    if !cx
        .global_mut::<SettingsRegistry>()
        .finish_release(generation)
    {
        return;
    }
    let _ = handle.update(cx, |_, window, _| window.remove_window());
    println!("PROVIDER_X_SMOKE settings_window=released");
}

fn codex_integration_is_active(status: &CodexConfigStatus) -> bool {
    matches!(status.receipt_phase, Some(ReceiptPhase::Active { .. })) && status.managed_values_match
}
