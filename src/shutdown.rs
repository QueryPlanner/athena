//! Stop requests from the operating system: SIGINT from Ctrl-C in a
//! terminal, SIGTERM from `kill`, `docker stop`, systemd and most hosts.
//! The long-running transports (`serve`, `telegram`) treat both the same:
//! stop taking new work, finish the turns in flight, exit.

use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// One wait for the next stop signal.
pub type Wait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Both stop signals, registered for the life of the process.
pub struct Signals {
    interrupt: Signal,
    terminate: Signal,
}

impl Signals {
    /// Take over SIGINT and SIGTERM now, before any work starts. Until a
    /// handler is registered, either signal kills the process on the spot,
    /// so this must run before the transport announces it is ready. Keeping
    /// the same two handlers afterwards means a second signal is never lost
    /// between one wait and the next.
    pub fn listen() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
        })
    }

    /// The next SIGINT or SIGTERM, whichever comes first.
    pub async fn next(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }

    /// A function whose every call waits for the next signal, for callers
    /// that wait more than once: once to stop gracefully, again to quit.
    pub fn waiter(self) -> impl Fn() -> Wait + Send + Sync + 'static {
        let signals = Arc::new(tokio::sync::Mutex::new(self));
        move || {
            let signals = signals.clone();
            Box::pin(async move { signals.lock().await.next().await })
        }
    }
}
