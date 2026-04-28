#[cfg(not(feature = "gtk-ui"))]
mod headless_service;

#[cfg(feature = "gtk-ui")]
mod gtk_app;
#[cfg(feature = "gtk-ui")]
mod render_cache;

#[cfg(feature = "gtk-ui")]
fn main() -> anyhow::Result<()> {
    gtk_app::run()
}

#[cfg(not(feature = "gtk-ui"))]
fn main() -> anyhow::Result<()> {
    headless_service::run()
}
