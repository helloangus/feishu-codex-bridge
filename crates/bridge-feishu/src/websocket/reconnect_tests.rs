use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[test]
fn partial_pong_preserves_retry_policy_and_invalid_update_is_atomic() {
    let mut config = ClientConfig {
        reconnect_count: 3,
        reconnect_interval: 7,
        ..ClientConfig::default()
    };
    assert!(config.update(br#"{"PingInterval":5}"#).is_ok());
    assert_eq!(config.reconnect_count, 3);
    assert_eq!(config.reconnect_interval, 7);
    assert_eq!(config.ping_interval, 5);
    assert!(
        config
            .update(br#"{"PingInterval":0,"ReconnectCount":9}"#)
            .is_err()
    );
    assert_eq!(config.reconnect_count, 3);
    assert_eq!(config.ping_interval, 5);
}

#[tokio::test(start_paused = true)]
async fn retry_limit_counts_short_connections_and_rediscovers_each_time() {
    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let result = reconnect(&tx, &cancel, move |config| {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            config.reconnect_count = 2;
            config.reconnect_nonce = 0;
            config.reconnect_interval = 1;
            Ok((false, Err(Error::Transport)))
        })
    })
    .await;
    assert!(matches!(result, Err(Error::Exhausted)));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let mut states = 0;
    while rx.try_recv().is_ok() {
        states += 1;
    }
    assert_eq!(states, 3);
}
#[tokio::test]
async fn closed_socket_is_replaced_with_new_endpoint_and_session()
-> Result<(), Box<dyn std::error::Error>> {
    let (tx, mut rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let attempts = count.clone();
    let events = tx.clone();
    let task = tokio::spawn(async move {
        reconnect(&tx,&stop,move |config| {
                let n=attempts.fetch_add(1,Ordering::SeqCst);let events=events.clone();
                Box::pin(async move {
                    // Each attempt receives fresh discovery credentials and a separate socket.
                    let endpoint=endpoint(json!({"code":0,"data":{"URL":format!("wss://example.invalid/ws?device_id=d{n}&service_id={}",n+1),"ClientConfig":{"ReconnectCount":2,"ReconnectNonce":0,"ReconnectInterval":1}}}).to_string().as_bytes())?;
                    *config=endpoint.config;
                    let (client,server)=tokio::io::duplex(8192);
                    let client=WebSocketStream::from_raw_socket(client,tokio_tungstenite::tungstenite::protocol::Role::Client,None).await;
                    let mut server=WebSocketStream::from_raw_socket(server,tokio_tungstenite::tungstenite::protocol::Role::Server,None).await;
                    connection(&events,ConnectionState::Connected)?;
                    let session_cancel=CancellationToken::new();let peer_cancel=session_cancel.clone();
                    let peer=async move {
                        let Some(Ok(Message::Binary(bytes)))=server.next().await else {return Err(Error::Protocol);};
                        assert_eq!(Frame::parse(&bytes)?.service,Some((n+1) as i32));
                        if n==0 {server.close(None).await.map_err(|_|Error::Transport)?;} else {peer_cancel.cancel();}
                        Ok::<_,Error>(())
                    };
                    let (result,peer)=tokio::join!(session(client,endpoint.service,config,events,session_cancel),peer);peer?;
                    if n==1 {return Err(Error::Authentication);} // Terminal fixture disposition after reconnection.
                    Ok((false,result))
                })
            }).await
    });
    assert!(matches!(
        timeout(Duration::from_secs(5), task).await??,
        Err(Error::Authentication)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let mut connected = 0;
    let mut reconnecting = 0;
    while let Ok(received) = rx.try_recv() {
        match received.event {
            Event::Connection {
                state: ConnectionState::Connected,
            } => connected += 1,
            Event::Connection {
                state: ConnectionState::Reconnecting,
            } => reconnecting += 1,
            _ => {}
        }
    }
    assert_eq!((connected, reconnecting), (2, 1));
    Ok(())
}
#[tokio::test]
async fn permanent_errors_do_not_retry_and_cancel_interrupts_pending_dial() {
    let (tx, _rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let result = reconnect(&tx, &cancel, |_| {
        Box::pin(async { Err(Error::Authentication) })
    })
    .await;
    assert!(matches!(result, Err(Error::Authentication)));
    let stop = cancel.clone();
    let task =
        tokio::spawn(
            async move { reconnect(&tx, &stop, |_| Box::pin(std::future::pending())).await },
        );
    tokio::task::yield_now().await;
    cancel.cancel();
    assert!(matches!(
        timeout(Duration::from_secs(1), task).await,
        Ok(Ok(Ok(())))
    ));
}
#[tokio::test(start_paused = true)]
async fn stable_connection_resets_failure_budget_and_backoff_is_cancellable() {
    let (tx, _rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let counter = Arc::new(AtomicUsize::new(0));
    let count = counter.clone();
    let result = reconnect(&tx, &cancel, move |config| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        let stop = stop.clone();
        Box::pin(async move {
            config.reconnect_count = 1;
            config.reconnect_nonce = 0;
            config.reconnect_interval = 120;
            if n == 2 {
                stop.cancel();
            }
            Ok((n == 1, Err(Error::Transport)))
        })
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}
