//! Native long connection. Reconnects transport only; application work is never replayed here.
pub mod wire;
use crate::{
    ingress::{Acceptance, ConnectionState, Event, MAX_FRAME_BYTES, Received},
    proxy::Policy,
};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{Duration, Instant, timeout},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use wire::{Fragments, Frame};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Every network-stage failure in this crate: DNS, dial, HTTP CONNECT or
    /// SOCKS5 handshake (see `proxy.rs`), WebSocket frames and reads. Proxy
    /// misconfiguration has its own variant; authentication has its own.
    #[error("飞书长连接网络失败")]
    Transport,
    #[error("飞书长连接协议无效")]
    Protocol,
    #[error("飞书长连接认证或权限失败")]
    Authentication,
    #[error("飞书长连接事件队列已满或关闭")]
    Overloaded,
    #[error("飞书长连接重试次数已用完")]
    Exhausted,
    #[error("飞书代理配置无效或不支持")]
    Proxy,
}
#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ClientConfig {
    pub reconnect_count: i32,
    pub reconnect_interval: u64,
    pub reconnect_nonce: u64,
    pub ping_interval: u64,
}
impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            reconnect_count: -1,
            reconnect_interval: 120,
            reconnect_nonce: 30,
            ping_interval: 120,
        }
    }
}
impl ClientConfig {
    fn update(&mut self, payload: &[u8]) -> Result<(), Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Update {
            reconnect_count: Option<i32>,
            reconnect_interval: Option<u64>,
            reconnect_nonce: Option<u64>,
            ping_interval: Option<u64>,
        }
        let update: Update = serde_json::from_slice(payload).map_err(|_| Error::Protocol)?;
        let mut candidate = self.clone();
        if let Some(value) = update.reconnect_count {
            candidate.reconnect_count = value;
        }
        if let Some(value) = update.reconnect_interval {
            candidate.reconnect_interval = value;
        }
        if let Some(value) = update.reconnect_nonce {
            candidate.reconnect_nonce = value;
        }
        if let Some(value) = update.ping_interval {
            candidate.ping_interval = value;
        }
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }
    fn validate(&self) -> Result<(), Error> {
        if !(-1..=10000).contains(&self.reconnect_count)
            || !(1..=3600).contains(&self.reconnect_interval)
            || self.reconnect_nonce > 300
            || !(1..=3600).contains(&self.ping_interval)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
pub struct Endpoint {
    pub url: Url,
    pub service: i32,
    pub config: ClientConfig,
}

/// A connection shorter than this never resets the retry budget: only a
/// genuinely stable session proves the failure was transient.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
/// Pending frame-receipt tasks before new events are NAKed (backpressure).
const MAX_PENDING_REPLIES: usize = 64;
/// Budget for one socket write and for one event receipt round-trip.
const WRITE_BUDGET: Duration = Duration::from_secs(1);
/// A missing pong for this long kills the session even if pings still go out.
fn pong_deadline(ping_interval: u64) -> Duration {
    Duration::from_secs(ping_interval.saturating_mul(2) + 5)
}

/// One connection attempt's result: how long it stayed up, and how it ended.
type AttemptResult = (bool, Result<(), Error>);
type Attempt<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<AttemptResult, Error>> + Send + 'a>>;
pub fn endpoint(bytes: &[u8]) -> Result<Endpoint, Error> {
    #[derive(Deserialize)]
    struct Response {
        code: i64,
        data: Option<Data>,
    }
    #[derive(Deserialize)]
    struct Data {
        #[serde(rename = "URL")]
        url: String,
        #[serde(rename = "ClientConfig")]
        config: Option<ClientConfig>,
    }
    if bytes.len() > 64 * 1024 {
        return Err(Error::Protocol);
    }
    let response: Response = serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?;
    match response.code {
        0 => {}
        1 | 1000040343 => return Err(Error::Transport),
        _ => return Err(Error::Authentication),
    }
    let data = response.data.ok_or(Error::Protocol)?;
    if data.url.len() > 16384 {
        return Err(Error::Protocol);
    }
    let url = Url::parse(&data.url).map_err(|_| Error::Protocol)?;
    if url.scheme() != "wss"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Protocol);
    }
    let values: Vec<_> = url.query_pairs().collect();
    let services: Vec<_> = values
        .iter()
        .filter(|(key, _)| key == "service_id")
        .collect();
    if services.len() != 1 || values.iter().filter(|(key, _)| key == "device_id").count() != 1 {
        return Err(Error::Protocol);
    }
    if values
        .iter()
        .any(|(key, value)| key == "device_id" && value.is_empty())
    {
        return Err(Error::Protocol);
    }
    let service = services[0].1.parse::<i32>().map_err(|_| Error::Protocol)?;
    if service < 0 {
        return Err(Error::Protocol);
    }
    let config = data.config.unwrap_or_default();
    config.validate()?;
    Ok(Endpoint {
        url,
        service,
        config,
    })
}

pub struct Client {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    policy: Policy,
    diagnostics: bridge_app::diagnostics::Diagnostics,
}
impl Client {
    pub fn new(
        app_id: String,
        app_secret: String,
        proxy: Option<&str>,
        diagnostics: bridge_app::diagnostics::Diagnostics,
    ) -> Result<Self, Error> {
        if app_id.is_empty() || app_secret.is_empty() {
            return Err(Error::Authentication);
        }
        let policy = Policy::from_env(proxy)?;
        let target = Url::parse("https://open.feishu.cn/callback/ws/endpoint")
            .map_err(|_| Error::Protocol)?;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10));
        if let Some(proxy) = policy.select(&target) {
            builder = builder.proxy(reqwest::Proxy::all(proxy.as_str()).map_err(|_| Error::Proxy)?);
        }
        let http = builder.build().map_err(|_| Error::Transport)?;
        Ok(Self {
            http,
            app_id,
            app_secret,
            policy,
            diagnostics,
        })
    }
    async fn discover(&self) -> Result<Endpoint, Error> {
        let response = self
            .http
            .post("https://open.feishu.cn/callback/ws/endpoint")
            .header("locale", "zh")
            .header("user-agent", "feishu-codex-bridge/rust")
            .json(&json!({"AppID":self.app_id,"AppSecret":self.app_secret}))
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
            return Err(Error::Authentication);
        }
        if !response.status().is_success() {
            return Err(Error::Transport);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Error::Transport)?;
            if bytes.len() + chunk.len() > 64 * 1024 {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        endpoint(&bytes)
    }
    pub async fn run(
        self,
        incoming: mpsc::Sender<Received>,
        cancel: CancellationToken,
    ) -> Result<(), Error> {
        let client = std::sync::Arc::new(self);
        reconnect(&client.diagnostics, &incoming, &cancel, |config| {
            let client = client.clone();
            let incoming = incoming.clone();
            let cancel = cancel.clone();
            Box::pin(async move {
                let endpoint = client.discover().await?;
                *config = endpoint.config;
                let socket = timeout(
                    Duration::from_secs(20),
                    crate::proxy::connect(&endpoint.url, &client.policy),
                )
                .await
                .map_err(|_| Error::Transport)??;
                connection(&incoming, ConnectionState::Connected)?;
                let started = Instant::now();
                let result = session(
                    socket,
                    endpoint.service,
                    config,
                    incoming.clone(),
                    cancel.clone(),
                    &client.diagnostics,
                )
                .await;
                Ok((started.elapsed() >= STABLE_CONNECTION, result))
            })
        })
        .await
    }
}

async fn reconnect<F>(
    diagnostics: &bridge_app::diagnostics::Diagnostics,
    incoming: &mpsc::Sender<Received>,
    cancel: &CancellationToken,
    mut attempt: F,
) -> Result<(), Error>
where
    F: for<'a> FnMut(&'a mut ClientConfig) -> Attempt<'a>,
{
    connection(incoming, ConnectionState::Starting)?;
    let mut config = ClientConfig::default();
    let mut attempts = 0_i32;
    loop {
        let result = tokio::select! {
            _=cancel.cancelled()=>return Ok(()),
            result = attempt(&mut config) => match result {
                Ok((stable, outcome)) => {
                    if stable {
                        attempts = 0;
                    }
                    outcome
                }
                Err(error) => Err(error),
            },
        };
        if cancel.is_cancelled() {
            return Ok(());
        }
        {
            use bridge_app::diagnostics::{Event, Status};
            let status = match &result {
                Ok(()) => Status::Ok,
                Err(Error::Transport) => Status::Transport,
                Err(Error::Protocol) => Status::Protocol,
                Err(Error::Authentication) => Status::Authentication,
                Err(Error::Proxy) => Status::Proxy,
                Err(Error::Overloaded) => Status::Overloaded,
                Err(Error::Exhausted) => Status::Exhausted,
            };
            diagnostics.emit(Event::Reconnect, status, None, attempts as usize);
        }
        match result {
            Err(Error::Authentication) | Err(Error::Proxy) | Err(Error::Overloaded) => {
                return result;
            }
            _ => {}
        }
        attempts = attempts.saturating_add(1);
        if config.reconnect_count >= 0 && attempts > config.reconnect_count {
            return Err(Error::Exhausted);
        }
        connection(incoming, ConnectionState::Reconnecting)?;
        let delay = if attempts == 1 {
            Duration::from_secs_f64(rand::random::<f64>() * config.reconnect_nonce as f64)
        } else {
            Duration::from_secs(config.reconnect_interval)
        };
        tokio::select! {_=cancel.cancelled()=>return Ok(()),_=tokio::time::sleep(delay)=>{}}
    }
}
fn connection(incoming: &mpsc::Sender<Received>, state: ConnectionState) -> Result<(), Error> {
    incoming
        .try_send(Received {
            event: Event::Connection { state },
            acceptance: None,
        })
        .map_err(|_| Error::Overloaded)
}

#[cfg(test)]
mod reconnect_tests;
async fn write<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    frame: Frame,
) -> Result<(), Error> {
    timeout(
        WRITE_BUDGET,
        socket.send(Message::Binary(frame.encode_to_vec().into())),
    )
    .await
    .map_err(|_| Error::Transport)?
    .map_err(|_| Error::Transport)
}

/// Session state threaded through frame handling.
struct SessionState<'a> {
    service: i32,
    config: &'a mut ClientConfig,
    fragments: &'a mut Fragments,
    replies: &'a mut JoinSet<Message>,
    incoming: &'a mpsc::Sender<Received>,
    last_pong: &'a mut Instant,
    next_ping: &'a mut Instant,
}

/// One decoded binary frame: either a heartbeat control frame (method 0) or a
/// data frame carrying an event or card. The protocol multiplexes both kinds
/// over the same socket; `method` distinguishes them.
async fn handle_frame<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    frame: Frame,
    state: &mut SessionState<'_>,
) -> Result<(), Error> {
    if frame.service != Some(state.service) {
        return Err(Error::Protocol);
    }
    let kind = frame.header("type")?;
    // Method 0 is the heartbeat channel: pongs carry server-side config
    // updates that take effect on the live session.
    if frame.method == Some(0) {
        if kind == "pong" {
            *state.last_pong = Instant::now();
            if let Some(payload) = frame.payload.filter(|payload| !payload.is_empty()) {
                state.config.update(&payload)?;
                *state.next_ping = Instant::now() + Duration::from_secs(state.config.ping_interval);
            }
        }
        return Ok(());
    }
    if !matches!(kind, "event" | "card") {
        return Ok(());
    }
    let Some(frame) = state.fragments.push(frame, Instant::now())? else {
        return Ok(()); // partial: more fragments still coming
    };
    dispatch_event(socket, frame, state.replies, state.incoming).await
}

/// Deliver one fully assembled event frame: forward it with a receipt the
/// runtime settles, and answer Feishu's acknowledgment with the decision.
/// When the reply queue is saturated the frame is NAKed instead of queued.
async fn dispatch_event<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    mut frame: Frame,
    replies: &mut JoinSet<Message>,
    incoming: &mpsc::Sender<Received>,
) -> Result<(), Error> {
    let started = Instant::now();
    let event = wire::event(&frame.payload.take().unwrap_or_default());
    match event {
        Ok(Some(event)) => {
            let card = matches!(&event, Event::Card { .. });
            if replies.len() >= MAX_PENDING_REPLIES {
                return write(socket, frame.reply(false, started.elapsed())).await;
            }
            let (receipt, wait) = oneshot::channel();
            let sent = incoming
                .try_send(Received {
                    event,
                    acceptance: Some(Acceptance(receipt)),
                })
                .is_ok();
            replies.spawn(async move {
                let accepted = sent && matches!(timeout(WRITE_BUDGET, wait).await, Ok(Ok(true)));
                let reply = frame.reply_with_card(accepted, started.elapsed(), card);
                Message::Binary(reply.encode_to_vec().into())
            });
        }
        Ok(None) => write(socket, frame.reply(true, started.elapsed())).await?,
        Err(_) => write(socket, frame.reply(false, started.elapsed())).await?,
    }
    Ok(())
}
/// Public for deterministic in-memory WebSocket tests; one owner reads and writes the socket.
pub async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: WebSocketStream<S>,
    service: i32,
    config: &mut ClientConfig,
    incoming: mpsc::Sender<Received>,
    cancel: CancellationToken,
    diagnostics: &bridge_app::diagnostics::Diagnostics,
) -> Result<(), Error> {
    config.validate()?;
    let mut fragments = Fragments::default();
    let mut replies: JoinSet<Message> = JoinSet::new();
    let mut last_pong = Instant::now();
    let mut next_ping = Instant::now();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = timeout(WRITE_BUDGET, socket.close(None)).await;
                return Ok(());
            }
            _ = tokio::time::sleep_until(next_ping) => {
                if last_pong.elapsed() > pong_deadline(config.ping_interval) {
                    diagnostics.emit(
                        bridge_app::diagnostics::Event::HeartbeatTimeout,
                        bridge_app::diagnostics::Status::Transport,
                        None,
                        0,
                    );
                    return Err(Error::Transport);
                }
                write(&mut socket, Frame::ping(service)).await?;
                next_ping = Instant::now() + Duration::from_secs(config.ping_interval);
            }
            reply = replies.join_next(), if !replies.is_empty() => {
                let reply = reply.ok_or(Error::Protocol)?.map_err(|_| Error::Protocol)?;
                timeout(WRITE_BUDGET, socket.send(reply))
                    .await
                    .map_err(|_| Error::Transport)?
                    .map_err(|_| Error::Transport)?;
            }
            message = socket.next() => {
                let message = message.ok_or(Error::Transport)?.map_err(|_| Error::Transport)?;
                let bytes = match message {
                    Message::Binary(bytes) => bytes,
                    Message::Ping(bytes) => {
                        timeout(WRITE_BUDGET, socket.send(Message::Pong(bytes)))
                            .await
                            .map_err(|_| Error::Transport)?
                            .map_err(|_| Error::Transport)?;
                        continue;
                    }
                    Message::Pong(_) => continue,
                    Message::Close(_) => return Err(Error::Transport),
                    _ => return Err(Error::Protocol),
                };
                if bytes.len() > MAX_FRAME_BYTES {
                    return Err(Error::Protocol);
                }
                let frame = Frame::parse(&bytes)?;
                let mut state = SessionState {
                    service,
                    config,
                    fragments: &mut fragments,
                    replies: &mut replies,
                    incoming: &incoming,
                    last_pong: &mut last_pong,
                    next_ping: &mut next_ping,
                };
                handle_frame(&mut socket, frame, &mut state).await?;
            }
        }
    }
}
