#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() == Some(std::ffi::OsStr::new("--prepare-ui-bundle")) {
        let root = arguments
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing UI bundle directory"))?;
        anyhow::ensure!(arguments.next().is_none(), "unexpected UI bundle argument");
        provider_x_app::desktop::prepare_ui_bundle(std::path::Path::new(&root))?;
        return Ok(());
    }
    provider_x_app::desktop::run()
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("provider-x v1 is available on macOS only")
}
