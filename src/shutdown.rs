//! The signals that stop lurker: SIGTERM, which Docker sends when it stops
//! or replaces the container, and SIGINT, from Ctrl+C in a terminal.

use std::io;

use tokio::signal::unix::{Signal, SignalKind, signal};

pub struct Shutdown {
    terminate: Signal,
    interrupt: Signal,
}

impl Shutdown {
    /// Starts listening for the signals, so none is missed between now and
    /// the first call to `recv`.
    pub fn new() -> io::Result<Shutdown> {
        Ok(Shutdown {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
        })
    }

    /// Waits for SIGTERM or SIGINT. Safe to cancel, as in `tokio::select!`.
    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }
}
