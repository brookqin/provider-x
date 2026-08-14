#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSStatusBar, NSVariableStatusItemLength,
    };
    use objc2_foundation::NSString;

    let marker = MainThreadMarker::new().ok_or_else(|| anyhow::anyhow!("not on main thread"))?;
    let application = NSApplication::sharedApplication(marker);
    anyhow::ensure!(
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory),
        "AppKit rejected Accessory policy"
    );

    let status_bar = NSStatusBar::systemStatusBar();
    let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
    let button = status_item
        .button(marker)
        .ok_or_else(|| anyhow::anyhow!("status item has no button"))?;
    button.setTitle(&NSString::from_str("PX"));
    println!("PROVIDER_X_NATIVE_SMOKE tray=ready activation_policy=accessory");

    application.run();
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("native status item benchmark is available on macOS only")
}
