rust_i18n::i18n!("locales", fallback = "en");

// rust-i18n expands locale resources at compile time without emitting Cargo dependency metadata.
// Keep them in rustc's dep-info so copy-only app packaging cannot reuse stale localized strings.
const _: &str = include_str!("../locales/en.yml");
const _: &str = include_str!("../locales/zh-CN.yml");

pub mod codex_config;
pub mod control_plane;
#[cfg(target_os = "macos")]
pub mod desktop;
pub(crate) mod localization;
#[cfg(target_os = "macos")]
pub mod platform;
#[cfg(target_os = "macos")]
mod runtime;
#[cfg(target_os = "macos")]
mod runtime_log;
pub mod storage;
