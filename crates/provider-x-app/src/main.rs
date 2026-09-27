#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    provider_x_app::desktop::run()
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("provider-x v1 is available on macOS only")
}
