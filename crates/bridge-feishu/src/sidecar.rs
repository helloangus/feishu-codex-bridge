//! SDK IPC pump. The application confirms only after durable admission or a
//! deliberate non-task disposition; merely receiving this channel is not ACK.
use crate::ingress::{Decoder, Event, MAX_FRAME_BYTES};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_util::{
    codec::{FramedRead, LinesCodec},
    sync::CancellationToken,
};

pub struct Received {
    pub event: Event,
    pub acceptance: Option<Acceptance>,
}
/// Dropping this handle sends a failed ACK. No reusable or cloneable receipt.
pub struct Acceptance(oneshot::Sender<bool>);
impl Acceptance {
    pub fn complete(self, accepted: bool) {
        let _ = self.0.send(accepted);
    }
}
#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("SDK IPC disconnected")]
    Disconnected,
    #[error("SDK IPC invalid")]
    Invalid,
    #[error("SDK IPC write failed")]
    Write,
}

/// Owns the sole stdout reader and stdin writer. No SDK or child is launched by
/// this function, allowing deterministic in-memory contract tests.
pub async fn pump<R, W>(
    reader: R,
    mut writer: W,
    epoch: String,
    incoming: mpsc::Sender<Received>,
    cancel: CancellationToken,
) -> Result<(), SidecarError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_FRAME_BYTES));
    let mut decoder = Decoder::new(epoch.clone());
    let mut sequence = 0_u64;
    loop {
        let frame = tokio::select! {
            biased;
            _=cancel.cancelled()=>return Ok(()),
            frame=lines.next()=>frame.ok_or(SidecarError::Disconnected)?.map_err(|_|SidecarError::Invalid)?,
        };
        let event = decoder
            .decode(frame.as_bytes())
            .map_err(|_| SidecarError::Invalid)?;
        let acceptance = if matches!(event, Event::Connection { .. }) {
            None
        } else {
            Some(oneshot::channel())
        };
        let (handle, wait) = match acceptance {
            Some((tx, rx)) => (Some(Acceptance(tx)), Some(rx)),
            None => (None, None),
        };
        // Never block protocol reads indefinitely on an unavailable application.
        let sent = incoming
            .try_send(Received {
                event,
                acceptance: handle,
            })
            .is_ok();
        if let Some(wait) = wait {
            let accepted = if sent {
                tokio::select! {
                    _=cancel.cancelled()=>false,
                    result=timeout(Duration::from_secs(1),wait)=>matches!(result,Ok(Ok(true))),
                }
            } else {
                false
            };
            let mut reply =
                json!({"version":1,"epoch":epoch,"sequence":sequence,"accepted":accepted})
                    .to_string();
            reply.push('\n');
            timeout(Duration::from_secs(1), async {
                writer.write_all(reply.as_bytes()).await?;
                writer.flush().await
            })
            .await
            .map_err(|_| SidecarError::Write)?
            .map_err(|_| SidecarError::Write)?;
        } else if !sent {
            return Err(SidecarError::Disconnected);
        }
        sequence = sequence.checked_add(1).ok_or(SidecarError::Invalid)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};
    #[tokio::test]
    async fn acceptance_controls_ack_and_dropped_receipt_rejects()
    -> Result<(), Box<dyn std::error::Error>> {
        timeout(Duration::from_secs(3),async {
            let (client,server)=tokio::io::duplex(8192);
            let (read,write)=tokio::io::split(client);
            let (server_read,mut server_write)=tokio::io::split(server);
            let mut replies=BufReader::new(server_read).lines();
            let (tx,mut rx)=mpsc::channel(1);let cancel=CancellationToken::new();
            let worker=tokio::spawn(pump(read,write,"g".into(),tx,cancel.clone()));
            for (sequence,accepted) in [(0,true),(1,false)] {
                let mut frame=json!({"version":1,"epoch":"g","sequence":sequence,"event":{"kind":"message","message_id":"m","user_id":"u","chat_id":"c","chat_type":"p2p","message_type":"text","content":{"text":"hello"}}}).to_string();frame.push('\n');
                server_write.write_all(frame.as_bytes()).await?;
                let received=rx.recv().await.ok_or("missing event")?;
                let receipt=received.acceptance.ok_or("missing receipt")?;
                if accepted {receipt.complete(true);} else {drop(receipt);}
                let reply:serde_json::Value=serde_json::from_str(&replies.next_line().await?.ok_or("missing ACK")?)?;
                assert_eq!(reply,json!({"version":1,"epoch":"g","sequence":sequence,"accepted":accepted}));
            }
            cancel.cancel();worker.await??;
            Ok::<_,Box<dyn std::error::Error>>(())
        }).await??;
        Ok(())
    }
}
