use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSApplication};
use objc2_foundation::NSString;

fn failure_message(_error: &anyhow::Error) -> (&'static str, String) {
    // Parser and upstream errors can contain credentials. Never display their raw text.
    (
        "startup_failed",
        rust_i18n::t!("app.startup.failed").to_string(),
    )
}

pub(crate) fn show_startup_failure(error: &anyhow::Error) {
    let (code, message) = failure_message(error);
    eprintln!("ProviderX startup failed: {code}");
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let alert = NSAlert::new(marker);
    alert.setMessageText(&NSString::from_str(&rust_i18n::t!("app.startup.title")));
    alert.setInformativeText(&NSString::from_str(&message));
    alert.addButtonWithTitle(&NSString::from_str(&rust_i18n::t!("app.tray.quit")));
    NSApplication::sharedApplication(marker).activate();
    alert.runModal();
}

pub(crate) fn show_upgrade_notice(backup: &std::path::Path) {
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let alert = NSAlert::new(marker);
    alert.setMessageText(&NSString::from_str(&rust_i18n::t!(
        "app.startup.upgrade_title"
    )));
    alert.setInformativeText(&NSString::from_str(&rust_i18n::t!(
        "app.startup.upgrade_notice",
        path = backup.display().to_string()
    )));
    alert.addButtonWithTitle(&NSString::from_str(&rust_i18n::t!("app.startup.configure")));
    NSApplication::sharedApplication(marker).activate();
    alert.runModal();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_startup_errors_are_redacted() {
        let secret = "synthetic-secret-do-not-display";
        let (code, message) = failure_message(&anyhow::anyhow!(secret));
        assert_eq!(code, "startup_failed");
        assert!(!message.contains(secret));
    }
}
