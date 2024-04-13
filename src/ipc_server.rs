use log::trace;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zbus::interface;

pub struct Remote {
    sender: broadcast::Sender<f64>,
    adjust: f64,
    cancel: CancellationToken,
}

impl Remote {
    pub fn new(
        sender: broadcast::Sender<f64>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            sender,
            cancel,
            adjust: 0.,
        }
    }
}

/// Server DBus definition, must be kept in sync with client!
#[interface(name = "com.cliffle.AmbientWalrus1")]
impl Remote {
    pub async fn adjust_by(&mut self, amt: f64) {
        self.adjust += amt;
        trace!("adjustment = {}", self.adjust);
        self.sender.send(self.adjust).ok();
    }

    pub async fn set_adjustment(&mut self, amt: f64) {
        self.adjust = amt;
        trace!("adjustment = {}", self.adjust);
        self.sender.send(self.adjust).ok();
    }

    pub async fn quit(&mut self) {
        // Using eprintln rather than log here to ensure that it makes it out
        // for debugging, regardless of log settings.
        eprintln!("shutting down due to IPC quit signal");
        self.cancel.cancel();
    }
}


