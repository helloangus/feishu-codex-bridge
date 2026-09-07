//! Bounded bidirectional JSONL transport, independent of child-process ownership.
use crate::{Envelope, RpcId, decode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum RpcError {
    #[error("Codex 连接已关闭")]
    Closed,
    #[error("Codex 请求超时；操作可能已经执行，不可自动重试")]
    Timeout,
    #[error("Codex 协议无效")]
    Protocol,
    #[error("Codex 输出积压，连接已停止")]
    Overloaded,
    #[error("拒绝旧连接的响应")]
    StaleConnection,
    #[error("Codex 返回错误（code={code}）")]
    Remote { code: i64 },
}

#[derive(Debug)]
pub struct ServerEvent {
    pub epoch: u64,
    pub envelope: Envelope,
}

struct Outgoing {
    message: Value,
    deadline: Instant,
    flushed: oneshot::Sender<Result<(), RpcError>>,
}

type Pending = Arc<Mutex<HashMap<RpcId, oneshot::Sender<Result<Value, RpcError>>>>>;

struct RequestGuard {
    pending: Pending,
    id: RpcId,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

#[derive(Clone)]
pub struct RpcClient {
    epoch: u64,
    next_id: Arc<AtomicU64>,
    pending: Pending,
    writer: mpsc::Sender<Outgoing>,
    slots: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl RpcClient {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        duration: Duration,
    ) -> Result<Value, RpcError> {
        let deadline = Instant::now() + duration;
        if self.cancel.is_cancelled() {
            return Err(RpcError::Closed);
        }
        let _permit = tokio::select! {
            _ = self.cancel.cancelled() => return Err(RpcError::Closed),
            permit = timeout_at(deadline, self.slots.acquire()) => permit.map_err(|_| RpcError::Timeout)?.map_err(|_| RpcError::Closed)?,
        };
        let id = RpcId::String(format!(
            "{}:{}",
            self.epoch,
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| RpcError::Closed)?
            .insert(id.clone(), sender);
        let _guard = RequestGuard {
            pending: self.pending.clone(),
            id: id.clone(),
        };
        self.write_before(
            json!({"id": id, "method": method, "params": params}),
            deadline,
        )
        .await?;
        tokio::select! {
            biased;
            answer = timeout_at(deadline, receiver) => answer.map_err(|_| RpcError::Timeout)?.map_err(|_| RpcError::Closed)?,
            _ = self.cancel.cancelled() => Err(RpcError::Closed),
        }
    }

    async fn write_before(&self, message: Value, deadline: Instant) -> Result<(), RpcError> {
        let (flushed, received) = oneshot::channel();
        let outgoing = Outgoing {
            message,
            deadline,
            flushed,
        };
        tokio::select! {
            _ = self.cancel.cancelled() => return Err(RpcError::Closed),
            sent = timeout_at(deadline, self.writer.send(outgoing)) => sent.map_err(|_| RpcError::Timeout)?.map_err(|_| RpcError::Closed)?,
        }
        tokio::select! {
            biased;
            result = timeout_at(deadline, received) => result.map_err(|_| RpcError::Timeout)?.map_err(|_| RpcError::Closed)?,
            _ = self.cancel.cancelled() => Err(RpcError::Closed),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.write_before(
            json!({"method": method, "params": params}),
            Instant::now() + Duration::from_secs(5),
        )
        .await
    }

    /// Fixed error text contains neither raw method names nor request payloads.
    pub async fn reject(&self, epoch: u64, id: RpcId) -> Result<(), RpcError> {
        if epoch != self.epoch {
            return Err(RpcError::StaleConnection);
        }
        self.write_before(json!({"id":id,"error":{"code":-32602,"message":"Unsupported or invalid bridge request"}}),
            Instant::now() + Duration::from_secs(5)).await
    }

    /// Keep server IDs opaque and reject responses from stale UI interactions.
    pub async fn reply(&self, epoch: u64, id: RpcId, result: Value) -> Result<(), RpcError> {
        if epoch != self.epoch {
            return Err(RpcError::StaleConnection);
        }
        self.write_before(
            json!({"id": id, "result": result}),
            Instant::now() + Duration::from_secs(5),
        )
        .await
    }
}

pub struct Connection {
    pub client: RpcClient,
    pub events: mpsc::Receiver<ServerEvent>,
    cancel: CancellationToken,
    reader: Option<JoinHandle<Result<(), RpcError>>>,
    writer: Option<JoinHandle<Result<(), RpcError>>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Connection {
    /// Exactly one reader and one writer; event backlog fails closed.
    pub fn new<R, W>(reader: R, writer: W, epoch: u64, max_frame: usize) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, out_rx) = mpsc::channel(32);
        let (event_tx, events) = mpsc::channel(256);
        let client = RpcClient {
            epoch,
            next_id: Arc::new(AtomicU64::new(0)),
            pending: pending.clone(),
            writer: out_tx,
            slots: Arc::new(Semaphore::new(32)),
            cancel: cancel.clone(),
        };
        let reader_cancel = cancel.clone();
        let reader_task = tokio::spawn(async move {
            let result = read_loop(
                reader,
                pending.clone(),
                event_tx,
                epoch,
                max_frame,
                reader_cancel.clone(),
            )
            .await;
            reader_cancel.cancel();
            if let Ok(mut pending) = pending.lock() {
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(result.clone().err().unwrap_or(RpcError::Closed)));
                }
            }
            result
        });
        let writer_cancel = cancel.clone();
        let writer_task = tokio::spawn(async move {
            let result = write_loop(writer, out_rx, writer_cancel.clone(), max_frame).await;
            writer_cancel.cancel();
            result
        });
        Self {
            client,
            events,
            cancel,
            reader: Some(reader_task),
            writer: Some(writer_task),
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), RpcError> {
        self.cancel.cancel();
        let mut failure = None;
        for handle in [self.reader.take(), self.writer.take()]
            .into_iter()
            .flatten()
        {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failure = Some(error),
                Err(_) => failure = Some(RpcError::Closed),
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: R,
    pending: Pending,
    events: mpsc::Sender<ServerEvent>,
    epoch: u64,
    max_frame: usize,
    cancel: CancellationToken,
) -> Result<(), RpcError> {
    let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(max_frame));
    loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            line = lines.next() => line.ok_or(RpcError::Closed)?.map_err(|_| RpcError::Protocol)?,
        };
        match decode(line.as_bytes(), max_frame).map_err(|_| RpcError::Protocol)? {
            Envelope::Response { id, result } => {
                if let Some(sender) = pending.lock().map_err(|_| RpcError::Closed)?.remove(&id) {
                    let _ = sender.send(Ok(result));
                }
            }
            Envelope::Error { id, error } => {
                if let Some(sender) = pending.lock().map_err(|_| RpcError::Closed)?.remove(&id) {
                    let _ = sender.send(Err(RpcError::Remote {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                    }));
                }
            }
            envelope => events
                .try_send(ServerEvent { epoch, envelope })
                .map_err(|_| RpcError::Overloaded)?,
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut messages: mpsc::Receiver<Outgoing>,
    cancel: CancellationToken,
    max_frame: usize,
) -> Result<(), RpcError> {
    loop {
        let message = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            message = messages.recv() => match message { Some(message) => message, None => return Ok(()) },
        };
        if message.flushed.is_closed() || Instant::now() >= message.deadline {
            let _ = message.flushed.send(Err(RpcError::Timeout));
            continue;
        }
        let mut bytes = serde_json::to_vec(&message.message).map_err(|_| RpcError::Protocol)?;
        if bytes.len() > max_frame {
            return Err(RpcError::Protocol);
        }
        bytes.push(b'\n');
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = timeout_at(message.deadline.min(Instant::now() + Duration::from_secs(5)), async { writer.write_all(&bytes).await?; writer.flush().await }) => {
                let result = result.map_err(|_| RpcError::Timeout).and_then(|value| value.map_err(|_| RpcError::Closed));
                let _ = message.flushed.send(result.clone());
                result?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader, duplex, split};
    use tokio::time::timeout;

    #[tokio::test]
    async fn concurrent_queries_are_routed_by_id_not_arrival()
    -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = duplex(4096);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 4, 4096);
        let server = tokio::spawn(async move {
            let (read, mut write) = split(server);
            let mut lines = BufReader::new(read).lines();
            let first: Value =
                serde_json::from_str(&lines.next_line().await?.ok_or("missing request")?)?;
            let second: Value =
                serde_json::from_str(&lines.next_line().await?.ok_or("missing request")?)?;
            for request in [second, first] {
                let response = json!({"id": request["id"], "result": request["params"]});
                write.write_all(format!("{response}\n").as_bytes()).await?;
            }
            // Wait for the client to close, so EOF cannot race valid replies.
            let _ = lines.next_line().await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (first, second) = tokio::join!(
            connection
                .client
                .request("one", json!({"n":1}), Duration::from_secs(2)),
            connection
                .client
                .request("two", json!({"n":2}), Duration::from_secs(2))
        );
        assert_eq!(first?, json!({"n":1}));
        assert_eq!(second?, json!({"n":2}));
        connection.shutdown().await?;
        assert!(server.await?.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn timeout_cleans_pending_and_stale_replies_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let (client, _server) = duplex(4096);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 2, 4096);
        assert_eq!(
            connection
                .client
                .request("slow", json!({}), Duration::from_millis(10))
                .await,
            Err(RpcError::Timeout)
        );
        assert!(
            connection
                .client
                .pending
                .lock()
                .map_err(|_| "poisoned")?
                .is_empty()
        );
        assert_eq!(
            connection
                .client
                .reply(1, RpcId::Integer(1), json!({"decision":"accept"}))
                .await,
            Err(RpcError::StaleConnection)
        );
        connection.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn eof_wakes_pending_requests() -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = duplex(4096);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 1, 4096);
        let query = connection.client.clone();
        let request = tokio::spawn(async move {
            query
                .request("query", json!({}), Duration::from_secs(60))
                .await
        });
        drop(server);
        assert_eq!(
            timeout(Duration::from_secs(1), request).await??,
            Err(RpcError::Closed)
        );
        let _ = connection.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn server_requests_keep_string_id_and_epoch() -> Result<(), Box<dyn std::error::Error>> {
        let (client, mut server) = duplex(4096);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 7, 4096);
        server
            .write_all(
                b"{\"id\":\"question\",\"method\":\"item/tool/requestUserInput\",\"params\":{}}\n",
            )
            .await?;
        let event = timeout(Duration::from_secs(1), connection.events.recv())
            .await?
            .ok_or("missing event")?;
        assert_eq!(event.epoch, 7);
        assert!(matches!(
            event.envelope,
            Envelope::Request {
                id: RpcId::String(_),
                ..
            }
        ));
        connection.shutdown().await?;
        Ok(())
    }
    #[tokio::test]
    async fn cancelled_caller_does_not_leak_pending_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = duplex(4096);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 3, 4096);
        let query = connection.client.clone();
        let pending = tokio::spawn(async move {
            query
                .request("slow", json!({}), Duration::from_secs(30))
                .await
        });
        let mut lines = BufReader::new(server).lines();
        assert!(
            timeout(Duration::from_secs(1), lines.next_line())
                .await??
                .is_some()
        );
        pending.abort();
        let _ = pending.await;
        assert!(
            connection
                .client
                .pending
                .lock()
                .map_err(|_| "poisoned")?
                .is_empty()
        );
        connection.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn oversized_input_closes_connection_instead_of_growing_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        let (client, mut server) = duplex(1024);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 1, 32);
        server.write_all(&[b'x'; 128]).await?;
        assert!(
            timeout(Duration::from_secs(1), connection.events.recv())
                .await?
                .is_none()
        );
        assert_eq!(connection.shutdown().await, Err(RpcError::Protocol));
        Ok(())
    }

    #[tokio::test]
    async fn unconsumed_critical_events_fail_closed_at_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        let (client, mut server) = duplex(65536);
        let (read, write) = split(client);
        let mut connection = Connection::new(read, write, 1, 1024);
        for _ in 0..257 {
            server
                .write_all(b"{\"method\":\"turn/completed\",\"params\":{}}\n")
                .await?;
        }
        timeout(Duration::from_secs(1), connection.cancel.cancelled()).await?;
        assert_eq!(connection.shutdown().await, Err(RpcError::Overloaded));
        Ok(())
    }
}
