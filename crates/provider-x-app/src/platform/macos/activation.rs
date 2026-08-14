use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ActivationPolicyError {
    #[error("AppKit activation policy must be changed on the macOS main thread")]
    NotMainThread,

    #[error("AppKit rejected the Accessory activation policy")]
    Rejected,
}

/// Restores tray-app semantics after GPUI's macOS backend selects the Regular policy.
///
/// # Errors
///
/// Returns an error when called off the main thread or when `AppKit` rejects the policy change.
pub fn set_accessory_activation_policy() -> Result<(), ActivationPolicyError> {
    let marker = MainThreadMarker::new().ok_or(ActivationPolicyError::NotMainThread)?;
    let application = NSApplication::sharedApplication(marker);
    if application.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        Ok(())
    } else {
        Err(ActivationPolicyError::Rejected)
    }
}

#[must_use]
pub fn is_accessory_activation_policy() -> bool {
    let Some(marker) = MainThreadMarker::new() else {
        return false;
    };
    NSApplication::sharedApplication(marker).activationPolicy()
        == NSApplicationActivationPolicy::Accessory
}
