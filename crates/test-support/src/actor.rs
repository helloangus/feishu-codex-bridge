//! Harness binding a fake app-server process to the real serial runtime.
//!
//! One call spawns the fake process, forwards its typed events into the
//! runtime and returns the same handles the former per-test copies wired by
//! hand: input sender, event sender, cancellation token and the two task
//! handles used for shutdown assertions.

use bridge_app::{
    diagnostics::Diagnostics,
    events::Incoming,
    files::TaskFiles,
    messaging::Messenger,
    ports::{BackendError, Sandbox},
    runtime,
};
use bridge_codex::process::AppServer;
use std::future::poll_fn;
use std::pin::Pin;
use std::{collections::BTreeSet, error::Error, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Everything one runtime scenario needs to start an actor.
pub struct ActorConfig {
    /// Absolute path of the fake process, never resolved through `PATH`.
    pub executable: PathBuf,
    /// Arguments forwarded to the fake process.
    pub args: Vec<String>,
    /// Working directory handed to the fake process.
    pub server_cwd: PathBuf,
    /// Epoch handed to the app-server connection and the runtime.
    pub epoch: u64,
    /// Runtime workspace root and initial directory.
    pub root: PathBuf,
    pub directory: PathBuf,
    /// Authorized user IDs.
    pub allowed: BTreeSet<String>,
    pub open_access: bool,
    pub sandbox: Sandbox,
}

/// A running fake app-server plus the runtime actor consuming its events.
pub struct Actor {
    pub input: mpsc::Sender<runtime::Input>,
    pub events: mpsc::Sender<Result<Incoming, BackendError>>,
    pub cancel: CancellationToken,
    /// Runtime task; joined by the test to assert clean or fatal shutdowns.
    pub worker: tokio::task::JoinHandle<Result<(), runtime::RuntimeError>>,
    /// Event-forwarding task; owns the fake process and shuts it down.
    pub server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Actor {
    /// Cancel the runtime and join both tasks. Returns the worker's runtime
    /// result and the server task's join result; the server task itself stays
    /// joined through `server_result` because JoinHandle awaits by value.
    pub async fn stop(
        &mut self,
    ) -> (
        Result<Result<(), runtime::RuntimeError>, tokio::task::JoinError>,
        Result<std::io::Result<()>, tokio::task::JoinError>,
    ) {
        self.cancel.cancel();
        // Take the handles out; the caller drops `self` right after anyway.
        let mut worker = std::mem::replace(
            &mut self.worker,
            tokio::task::spawn(std::future::ready(Ok(()))),
        );
        let mut server = std::mem::replace(
            &mut self.server,
            tokio::task::spawn(std::future::ready(Ok(()))),
        );
        let worker_result = poll_fn(|cx| Pin::new(&mut worker).poll(cx)).await;
        let server_result = poll_fn(|cx| Pin::new(&mut server).poll(cx)).await;
        (worker_result, server_result)
    }
}

impl Actor {
    /// Spawn the fake process and the runtime actor bound to it.
    ///
    /// `store` and `messenger` are consumed by the runtime; callers keep their
    /// own typed clones for assertions after shutdown.
    pub async fn start(
        config: ActorConfig,
        store: Arc<dyn runtime::Store>,
        messenger: Arc<dyn Messenger>,
        diagnostics: Diagnostics,
    ) -> Result<Self, Box<dyn Error>> {
        let mut server = AppServer::spawn(
            &config.executable,
            &config.args,
            &config.server_cwd,
            config.epoch,
        )
        .await?;
        let backend = Arc::new(server.backend());
        let (input, input_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let forward_tx = event_tx.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = stop.cancelled() => break,
                    event = server.next_event() => event,
                };
                if forward_tx.send(event).await.is_err() {
                    break;
                }
            }
            server.shutdown().await
        });
        let worker = tokio::spawn(runtime::run(
            runtime::Settings {
                root: config.root,
                directory: config.directory,
                allowed: config.allowed,
                open_access: config.open_access,
                sandbox: config.sandbox,
                epoch: config.epoch,
            },
            diagnostics,
            backend,
            store,
            messenger,
            Arc::new(crate::files::IdleFiles) as Arc<dyn TaskFiles>,
            input_rx,
            event_rx,
            cancel.clone(),
        ));
        Ok(Self {
            input,
            events: event_tx,
            cancel,
            worker,
            server: forwarder,
        })
    }

    /// Send a plain chat text message and require admission.
    pub async fn send_text(&self, id: &str, user: &str, text: &str) -> Result<(), Box<dyn Error>> {
        crate::input::submit_text(&self.input, id, user, "chat", text).await
    }
}
