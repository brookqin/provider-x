use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ActivationPolicyError {
    #[error("AppKit activation policy must be changed on the macOS main thread")]
    NotMainThread,

    #[error("AppKit rejected the requested activation policy")]
    Rejected,
}

/// Restores tray-app semantics after GPUI's macOS backend selects the Regular policy.
///
/// # Errors
///
/// Returns an error when called off the main thread or when `AppKit` rejects the policy change.
pub fn set_accessory_activation_policy() -> Result<(), ActivationPolicyError> {
    set_dock_visible(false)
}

/// Changes Dock visibility without changing the menu-bar or window-close lifecycle.
///
/// # Errors
/// Returns an error off the main thread or when `AppKit` rejects the requested policy.
pub fn set_dock_visible(visible: bool) -> Result<(), ActivationPolicyError> {
    let marker = MainThreadMarker::new().ok_or(ActivationPolicyError::NotMainThread)?;
    let application = NSApplication::sharedApplication(marker);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    // AppKit can reject a no-op transition during startup, when GPUI is already Regular.
    if application.activationPolicy() == policy {
        return Ok(());
    }
    let was_active = application.isActive();
    let visible_windows: Vec<_> = application
        .windows()
        .iter()
        .filter(|window| window.isVisible())
        .map(|window| {
            let was_key = window.isKeyWindow();
            (window, was_key)
        })
        .collect();
    if !application.setActivationPolicy(policy) {
        return Err(ActivationPolicyError::Rejected);
    }
    // Switching to Accessory can hide the application. Restore only windows that were visible;
    // changing Dock visibility must not act like closing settings or reopen a hidden window.
    if !visible_windows.is_empty() {
        application.unhideWithoutActivation();
        for (window, was_key) in visible_windows {
            if was_key {
                window.makeKeyAndOrderFront(None);
            } else {
                window.orderFront(None);
            }
        }
        if was_active {
            application.activate();
        }
    }
    Ok(())
}

pub(crate) fn has_visible_window() -> bool {
    MainThreadMarker::new().is_some_and(|marker| {
        let application = NSApplication::sharedApplication(marker);
        !application.isHidden()
            && application
                .windows()
                .iter()
                .any(|window| window.isVisible())
    })
}

#[must_use]
pub fn is_accessory_activation_policy() -> bool {
    let Some(marker) = MainThreadMarker::new() else {
        return false;
    };
    NSApplication::sharedApplication(marker).activationPolicy()
        == NSApplicationActivationPolicy::Accessory
}
