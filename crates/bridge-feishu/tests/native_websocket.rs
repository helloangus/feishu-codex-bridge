//! Complete WebSocket frames over in-memory duplex streams: no network or real credentials.
use bridge_feishu::{
    ingress::{Event, Received},
    websocket::{
        self, ClientConfig, Error,
        wire::{self, Fragments, Frame, Header},
    },
};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use serde_json::{Value, json};
use tokio::{
    io::DuplexStream,
    sync::mpsc,
    time::{Duration, Instant, timeout},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};
use tokio_util::sync::CancellationToken;
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn cancellation_drops_pending_ack_and_full_queue_returns_failure() -> Result {
    for full in [true, false] {
        let (client, mut server) = pair().await;
        let (tx, mut rx) = mpsc::channel(1);
        if full {
            tx.send(Received {
                event: Event::Connection {
                    state: bridge_feishu::ingress::ConnectionState::Starting,
                },
                acceptance: None,
            })
            .await?;
        }
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let worker = tokio::spawn(async move {
            websocket::session(client, 1, &mut ClientConfig::default(), tx, stop).await
        });
        let _ = read(&mut server).await?;
        server
            .send(Message::Binary(
                frame("event", "m", message().to_string().into_bytes())
                    .encode_to_vec()
                    .into(),
            ))
            .await?;
        let pending = if full {
            let ack = read(&mut server).await?;
            assert_eq!(
                serde_json::from_slice::<Value>(&ack.payload.ok_or("payload")?)?["code"],
                500
            );
            None
        } else {
            Some(rx.recv().await.ok_or("missing event")?)
        };
        cancel.cancel();
        timeout(Duration::from_secs(2), worker).await???;
        if let Some(received) = pending {
            received.acceptance.ok_or("receipt")?.complete(true);
        }
        assert!(rx.try_recv().is_err() || full);
    }
    Ok(())
}
fn message() -> Value {
    json!({"schema":"2.0","header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_type":"user","sender_id":{"open_id":"owner"}},"message":{"message_id":"m","chat_id":"chat","chat_type":"p2p","message_type":"text","content":"{\"text\":\"hello\"}"}}})
}
fn card() -> Value {
    json!({"schema":"2.0","header":{"event_type":"card.action.trigger"},"event":{"operator":{"open_id":"owner"},"context":{"open_chat_id":"chat","open_message_id":"original-card"},"action":{"value":{"command":"/interaction","token":"opaque","choice":"run"}}}})
}
fn frame(kind: &str, id: &str, payload: Vec<u8>) -> Frame {
    Frame {
        seq_id: Some(3),
        log_id: Some(9),
        service: Some(1),
        method: Some(1),
        headers: vec![
            Header {
                key: "type".into(),
                value: kind.into(),
            },
            Header {
                key: "message_id".into(),
                value: id.into(),
            },
            Header {
                key: "sum".into(),
                value: "1".into(),
            },
            Header {
                key: "seq".into(),
                value: "0".into(),
            },
            Header {
                key: "trace_id".into(),
                value: "trace".into(),
            },
        ],
        payload: Some(payload),
        log_id_new: Some("log-new".into()),
        ..Frame::default()
    }
}
fn header(frame: &mut Frame, key: &str, value: &str) {
    if let Some(h) = frame.headers.iter_mut().find(|h| h.key == key) {
        h.value = value.into();
    }
}
async fn read(server: &mut WebSocketStream<DuplexStream>) -> Result<Frame> {
    loop {
        let msg = timeout(Duration::from_secs(3), server.next())
            .await?
            .ok_or("closed")??;
        if let Message::Binary(bytes) = msg {
            return Ok(Frame::parse(&bytes)?);
        }
    }
}
async fn pair() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
    let (client, server) = tokio::io::duplex(65536);
    let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    (client, server)
}
#[test]
fn protobuf_ping_matches_sdk_field_numbers_and_rejects_missing_required_fields() -> Result {
    // SDK pbbp2.Frame: SeqID=0, LogID=0, service=1, method=CONTROL, type=ping.
    let sdk = [
        8, 0, 16, 0, 24, 1, 32, 0, 42, 12, 10, 4, 116, 121, 112, 101, 18, 4, 112, 105, 110, 103,
    ];
    assert_eq!(Frame::ping(1).encode_to_vec(), sdk);
    assert_eq!(Frame::parse(&sdk)?.header("type")?, "ping");
    assert!(Frame::parse(&[]).is_err());
    let mut duplicate = Frame::ping(1);
    duplicate.headers.push(duplicate.headers[0].clone());
    assert!(Frame::parse(&duplicate.encode_to_vec()).is_err());
    Ok(())
}
#[test]
fn discovery_requires_secure_url_and_classifies_auth_and_transient_failures() -> Result {
    let data = json!({"code":0,"data":{"URL":"wss://msg-frontier.feishu.cn/ws?device_id=device&service_id=1","ClientConfig":{"PingInterval":30,"ReconnectInterval":2,"ReconnectNonce":1,"ReconnectCount":4}}});
    let endpoint = websocket::endpoint(data.to_string().as_bytes())?;
    assert_eq!(endpoint.service, 1);
    assert_eq!(endpoint.config.ping_interval, 30);
    assert!(matches!(
        websocket::endpoint(br#"{"code":1}"#),
        Err(Error::Transport)
    ));
    assert!(matches!(
        websocket::endpoint(br#"{"code":1000040344}"#),
        Err(Error::Authentication)
    ));
    for url in [
        "ws://host/?device_id=d&service_id=1",
        "wss://host/?service_id=1",
        "wss://host/?device_id=d&service_id=1&service_id=2",
    ] {
        let mut data = data.clone();
        data["data"]["URL"] = json!(url);
        assert!(websocket::endpoint(data.to_string().as_bytes()).is_err());
    }
    Ok(())
}
#[test]
fn fragments_accept_out_of_order_and_duplicates_but_reject_conflicts_and_expire() -> Result {
    let payload = message().to_string().into_bytes();
    let midpoint = payload.len() / 2;
    let mut first = frame("event", "m", payload[..midpoint].to_vec());
    header(&mut first, "sum", "2");
    let mut second = first.clone();
    header(&mut second, "seq", "1");
    second.payload = Some(payload[midpoint..].to_vec());
    let mut parts = Fragments::default();
    let now = Instant::now();
    assert!(parts.push(second.clone(), now)?.is_none());
    assert!(parts.push(second.clone(), now)?.is_none());
    assert_eq!(
        parts
            .push(first.clone(), now)?
            .ok_or("not assembled")?
            .payload,
        Some(payload)
    );
    assert!(parts.push(first.clone(), now)?.is_none());
    let mut conflict = first.clone();
    conflict.payload = Some(b"bad".to_vec());
    assert!(parts.push(conflict, now).is_err());
    assert!(parts.push(first.clone(), now)?.is_none());
    assert!(
        parts
            .push(second.clone(), now + Duration::from_secs(6))?
            .is_none()
    );
    header(&mut first, "sum", "999999");
    assert!(parts.push(first, now).is_err());
    Ok(())
}
#[test]
fn event_and_card_payloads_preserve_source_and_reject_invalid_known_events() -> Result {
    assert!(
        matches!(wire::event(message().to_string().as_bytes())?,Some(Event::Message {content,..}) if content["text"]=="hello")
    );
    assert!(
        matches!(wire::event(card().to_string().as_bytes())?,Some(Event::Card {message_id,action,..}) if message_id=="original-card" && action["token"]=="opaque")
    );
    let mut invalid = card();
    invalid["event"]["operator"]["open_id"] = json!("");
    assert!(wire::event(invalid.to_string().as_bytes()).is_err());
    assert!(wire::event(br#"{"header":{"event_type":"ignored"}}"#)?.is_none());
    Ok(())
}
#[tokio::test]
async fn event_and_card_ack_wait_for_admission_while_control_frames_continue() -> Result {
    timeout(Duration::from_secs(10), async {
        let (client, mut server) = pair().await;
        let (tx, mut rx) = mpsc::channel::<Received>(8);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let worker = tokio::spawn(async move {
            websocket::session(client, 1, &mut ClientConfig::default(), tx, stop).await
        });
        assert_eq!(read(&mut server).await?.header("type")?, "ping");
        for (kind, payload, accepted) in [
            ("event", message(), true),
            ("card", card(), true),
            ("event", card(), true),
            ("event", message(), false),
        ] {
            let sent = frame(kind, "m", payload.to_string().into_bytes());
            server
                .send(Message::Binary(sent.encode_to_vec().into()))
                .await?;
            let received = rx.recv().await.ok_or("no incoming")?;
            assert!(
                timeout(Duration::from_millis(10), server.next())
                    .await
                    .is_err()
            );
            // A pending application receipt must not prevent WebSocket control reads.
            server
                .send(Message::Ping(b"still-alive".to_vec().into()))
                .await?;
            assert!(matches!(
                server.next().await.ok_or("no pong")??,
                Message::Pong(_)
            ));
            let receipt = received.acceptance.ok_or("missing receipt")?;
            if accepted {
                receipt.complete(true);
            } else {
                drop(receipt);
            }
            let ack = read(&mut server).await?;
            assert_eq!(ack.seq_id, sent.seq_id);
            assert_eq!(ack.log_id_new, sent.log_id_new);
            let body: Value = serde_json::from_slice(&ack.payload.ok_or("payload")?)?;
            assert_eq!(body["code"], if accepted { 200 } else { 500 });
            if accepted && payload["header"]["event_type"] == "card.action.trigger" {
                assert_eq!(body["data"], "e30=");
            }
        }
        cancel.cancel();
        worker.await??;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}
#[tokio::test]
async fn receipt_timeout_and_malformed_payload_return_failed_ack() -> Result {
    timeout(Duration::from_secs(10), async {
        let (client, mut server) = pair().await;
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let worker = tokio::spawn(async move {
            websocket::session(client, 1, &mut ClientConfig::default(), tx, stop).await
        });
        let _ = read(&mut server).await?;
        server
            .send(Message::Binary(
                frame("event", "slow", message().to_string().into_bytes())
                    .encode_to_vec()
                    .into(),
            ))
            .await?;
        let held = rx.recv().await.ok_or("receipt")?;
        let ack = read(&mut server).await?;
        assert_eq!(
            serde_json::from_slice::<Value>(&ack.payload.ok_or("payload")?)?["code"],
            500
        );
        drop(held);
        server
            .send(Message::Binary(
                frame("card", "bad", b"{".to_vec()).encode_to_vec().into(),
            ))
            .await?;
        let ack = read(&mut server).await?;
        assert_eq!(
            serde_json::from_slice::<Value>(&ack.payload.ok_or("payload")?)?["code"],
            500
        );
        assert!(rx.try_recv().is_err());
        cancel.cancel();
        worker.await??;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await?
}
#[tokio::test]
async fn pong_configuration_changes_ping_schedule_and_missing_pong_disconnects() -> Result {
    let (client, mut server) = pair().await;
    let (tx, _rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let worker = tokio::spawn(async move {
        websocket::session(client, 1, &mut ClientConfig::default(), tx, stop).await
    });
    let _ = read(&mut server).await?;
    let mut pong = Frame::ping(1);
    header(&mut pong, "type", "pong");
    pong.payload = Some(br#"{"PingInterval":1}"#.to_vec());
    server
        .send(Message::Binary(pong.encode_to_vec().into()))
        .await?;
    let ping = read(&mut server).await?;
    assert_eq!(ping.header("type")?, "ping");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(9)).await;
    tokio::time::resume();
    assert!(matches!(
        timeout(Duration::from_secs(3), worker).await??,
        Err(Error::Transport)
    ));
    Ok(())
}
