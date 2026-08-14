use std::{cell::Cell, io::Cursor};

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    OpenSettings,
    ManageCodexIntegration,
    Quit,
}

pub struct MacTrayController {
    _icon: TrayIcon,
    listener_address: String,
    listener_status: MenuItem,
    codex_status: MenuItem,
    manage_codex: MenuItem,
    open_settings: MenuItem,
    quit: MenuItem,
    codex_enabled: Cell<bool>,
    open_settings_id: MenuId,
    manage_codex_id: MenuId,
    quit_id: MenuId,
}

impl MacTrayController {
    /// Creates the status item. Call this only after the macOS application event loop has started.
    ///
    /// # Errors
    ///
    /// Returns an error when the native menu or status item cannot be created.
    pub fn new(
        listener_address: impl std::fmt::Display,
        codex_enabled: bool,
    ) -> anyhow::Result<Self> {
        let listener_address = listener_address.to_string();
        let menu = Menu::new();
        let listener_status = MenuItem::new(
            &rust_i18n::t!("app.tray.router_running", address = &listener_address),
            false,
            None,
        );
        let codex_status = MenuItem::new(codex_status_text(codex_enabled), false, None);
        let open_settings = MenuItem::new(&rust_i18n::t!("app.tray.open_settings"), true, None);
        let manage_codex = MenuItem::new(manage_codex_text(codex_enabled), true, None);
        let quit = MenuItem::new(&rust_i18n::t!("app.tray.quit"), true, None);
        menu.append(&listener_status)?;
        menu.append(&codex_status)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&open_settings)?;
        menu.append(&manage_codex)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit)?;

        let open_settings_id = open_settings.id().clone();
        let manage_codex_id = manage_codex.id().clone();
        let quit_id = quit.id().clone();
        let icon = TrayIconBuilder::new()
            .with_tooltip("ProviderX")
            .with_icon(provider_x_icon()?)
            .with_icon_as_template(true)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_menu_on_right_click(true)
            .build()?;

        Ok(Self {
            _icon: icon,
            listener_address,
            listener_status,
            codex_status,
            manage_codex,
            open_settings,
            quit,
            codex_enabled: Cell::new(codex_enabled),
            open_settings_id,
            manage_codex_id,
            quit_id,
        })
    }

    pub fn set_codex_enabled(&self, enabled: bool) {
        self.codex_enabled.set(enabled);
        self.codex_status.set_text(codex_status_text(enabled));
        self.manage_codex.set_text(manage_codex_text(enabled));
        self.manage_codex.set_enabled(true);
    }

    pub fn set_codex_operation_pending(&self, enabling: bool) {
        self.codex_status.set_text(if enabling {
            rust_i18n::t!("app.tray.codex_enabling").to_string()
        } else {
            rust_i18n::t!("app.tray.codex_disabling").to_string()
        });
        self.manage_codex.set_enabled(false);
    }

    pub fn refresh_locale(&self) {
        self.listener_status.set_text(&rust_i18n::t!(
            "app.tray.router_running",
            address = &self.listener_address
        ));
        self.open_settings
            .set_text(&rust_i18n::t!("app.tray.open_settings"));
        self.quit.set_text(&rust_i18n::t!("app.tray.quit"));
        self.set_codex_enabled(self.codex_enabled.get());
    }

    #[must_use]
    pub fn next_command(&self) -> Option<TrayCommand> {
        // The native tray menu handles both left and right clicks. Drain raw click events so they
        // cannot accumulate, but only explicit menu selections produce application commands.
        while TrayIconEvent::receiver().try_recv().is_ok() {}

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.open_settings_id {
                return Some(TrayCommand::OpenSettings);
            }
            if event.id == self.manage_codex_id {
                return Some(TrayCommand::ManageCodexIntegration);
            }
            if event.id == self.quit_id {
                return Some(TrayCommand::Quit);
            }
        }
        None
    }
}

fn codex_status_text(enabled: bool) -> String {
    if enabled {
        rust_i18n::t!("app.tray.codex_enabled").to_string()
    } else {
        rust_i18n::t!("app.tray.codex_disabled").to_string()
    }
}

fn manage_codex_text(enabled: bool) -> String {
    if enabled {
        rust_i18n::t!("app.tray.disable_codex").to_string()
    } else {
        rust_i18n::t!("app.tray.enable_codex").to_string()
    }
}

fn provider_x_icon() -> anyhow::Result<Icon> {
    let (rgba, width, height) = decode_tray_icon(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/resources/tray.png"
    )))?;
    Icon::from_rgba(rgba, width, height).map_err(Into::into)
}

fn decode_tray_icon(png_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let mut reader = decoder.read_info()?;
    let output_size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow::anyhow!("tray icon PNG exceeds decoder limits"))?;
    let mut rgba = vec![0; output_size];
    let info = reader.next_frame(&mut rgba)?;

    anyhow::ensure!(
        info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight,
        "tray icon must be an 8-bit RGBA PNG"
    );
    rgba.truncate(info.buffer_size());
    Ok((rgba, info.width, info.height))
}

#[cfg(test)]
mod tests {
    use super::{codex_status_text, decode_tray_icon, manage_codex_text};

    #[test]
    fn embedded_tray_icon_is_retina_rgba() {
        let (rgba, width, height) = decode_tray_icon(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/tray.png"
        )))
        .unwrap();

        assert_eq!((width, height), (36, 36));
        assert_eq!(rgba.len(), 36 * 36 * 4);
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn integration_menu_copy_preserves_safe_disable_semantics() {
        rust_i18n::set_locale("zh-CN");
        assert_eq!(codex_status_text(true), "Codex 集成：已启用");
        assert_eq!(codex_status_text(false), "Codex 集成：未启用");
        assert!(manage_codex_text(true).contains("保持 Router 运行"));
        assert!(manage_codex_text(false).ends_with('…'));
        assert_eq!(rust_i18n::t!("app.tray.quit"), "退出");

        rust_i18n::set_locale("en");
        assert_eq!(codex_status_text(true), "Codex integration: Enabled");
        assert_eq!(manage_codex_text(false), "Enable Codex Integration…");
    }
}
