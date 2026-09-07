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
    group: Option<rustix::process::Pid>,
}

impl Drop for AppServer {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
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
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| BackendError::Disconnected)?;
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .filter(|id| *id > 1)
            .and_then(rustix::process::Pid::from_raw);
        let input = child.stdin.take().ok_or(BackendError::Disconnected)?;
        let output = child.stdout.take().ok_or(BackendError::Disconnected)?;
        let connection = Connection::new(output, input, epoch, 8 * 1024 * 1024);
        let mut server = Self {
            connection,
            child,
            group,
        };
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
    /// The child has its own process group; tools remaining after stdin closes
    /// are terminated too, without signaling the bridge's process group.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.connection.shutdown().await;
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
        }
        // Keep the unreaped child identity reserved while cleaning its group.
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(group) = self.group.take() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
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
        let exited = self
            .child
            .try_wait()
            .map_err(|_| RpcError::Closed)?
            .is_some();
        if exited {
            if let Some(group) = self.group.take() {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
        }
        Ok(exited)
    }
}
