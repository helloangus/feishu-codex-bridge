//! Child process ownership, isolated from the RPC stream and application rules.
use crate::{
    backend::CodexBackend,
    transport::{Connection, RpcError},
};
use bridge_app::ports::BackendError;
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::{
    process::{Child, Command},
    time::timeout,
};

pub struct AppServer {
    pub connection: Connection,
    child: Child,
}

impl AppServer {
    /// No shell; callers supply executable and individual argument strings.
    pub async fn spawn(
        executable: &Path,
        args: &[String],
        cwd: &Path,
        epoch: u64,
    ) -> Result<Self, BackendError> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| BackendError::Disconnected)?;
        let input = child.stdin.take().ok_or(BackendError::Disconnected)?;
        let output = child.stdout.take().ok_or(BackendError::Disconnected)?;
        let connection = Connection::new(output, input, epoch, 8 * 1024 * 1024);
        let mut server = Self { connection, child };
        if let Err(error) = CodexBackend::new(server.connection.client.clone())
            .initialize()
            .await
        {
            let _ = server.shutdown().await;
            return Err(error);
        }
        Ok(server)
    }

    /// Close stdin first, then kill and reap this owned child if it won't exit.
    /// Full process-group supervision is provided by the service migration.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.connection.shutdown().await;
        match timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(result) => {
                result?;
            }
            Err(_) => {
                self.child.start_kill()?;
                self.child.wait().await?;
            }
        }
        Ok(())
    }

    /// One consumer owns this stream. Unknown notifications are skipped;
    /// unsupported requests receive an explicit error before failure propagates.
    /// No Feishu I/O runs on the transport reader task.
    pub async fn next_event(&mut self) -> Result<bridge_app::events::Incoming, BackendError> {
        loop {
            let event = self
                .connection
                .events
                .recv()
                .await
                .ok_or(BackendError::Disconnected)?;
            if event.epoch != self.connection.client.epoch() {
                return Err(BackendError::Incompatible);
            }
            match event.envelope {
                crate::Envelope::Notification { method, params } => {
                    if let Some(event) = crate::events::notification(event.epoch, &method, params)?
                    {
                        return Ok(bridge_app::events::Incoming::Notification(event));
                    }
                }
                crate::Envelope::Request { id, method, params } => {
                    let (request, reply) = crate::requests::prepare(
                        self.connection.client.clone(),
                        event.epoch,
                        id,
                        &method,
                        params,
                    )
                    .await?;
                    return Ok(bridge_app::events::Incoming::Request { request, reply });
                }
                _ => return Err(BackendError::Incompatible),
            }
        }
    }

    pub fn backend(&self) -> CodexBackend {
        CodexBackend::new(self.connection.client.clone())
    }
    pub fn is_disconnected(&mut self) -> Result<bool, RpcError> {
        Ok(self
            .child
            .try_wait()
            .map_err(|_| RpcError::Closed)?
            .is_some())
    }
}
