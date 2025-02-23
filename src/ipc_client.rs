use zbus::proxy;

/// Client interface definition. Must be kept in sync with server definition!
#[proxy(
    default_service = "com.cliffle.AmbientWalrus",
    default_path = "/com/cliffle/AmbientWalrus",
    interface = "com.cliffle.AmbientWalrus1",
)]
pub(crate) trait Remote {
    async fn adjust_by(&self, amt: f64) -> zbus::Result<()>;
    async fn set_adjustment(&self, amt: f64) -> zbus::Result<()>;
    async fn quit(&self) -> zbus::Result<()>;
}
