//! Stop signals: SIGINT from Ctrl-C, SIGTERM from `kill`, `docker stop` and
//! systemd. `serve` and `telegram` stop gracefully on either.

use anyhow::{Context, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::signal::unix::{SignalKind, signal};

/// One wait for the next stop signal.
pub type Wait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Take over SIGINT and SIGTERM now, and return a function whose every call
/// waits for the next one.
///
/// Call it before announcing that the transport is ready: until a handler
/// is registered, either signal kills the process outright. The handlers
/// then stay registered, so a signal between two waits is not lost.
pub fn listen() -> Result<impl Fn() -> Wait + Send + Sync + 'static> {
    let interrupt = signal(SignalKind::interrupt()).context("listening for SIGINT")?;
    let terminate = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    let signals = Arc::new(tokio::sync::Mutex::new((interrupt, terminate)));
    Ok(move || {
        let signals = signals.clone();
        Box::pin(async move {
            let (interrupt, terminate) = &mut *signals.lock().await;
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        }) as Wait
    })
}
