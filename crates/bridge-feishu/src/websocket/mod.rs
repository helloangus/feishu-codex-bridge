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
}
impl Client {
    pub fn new(app_id: String, app_secret: String, proxy: Option<&str>) -> Result<Self, Error> {
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
        reconnect(&incoming, &cancel, |config| {
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
                )
                .await;
                Ok((started.elapsed() >= Duration::from_secs(60), result))
            })
        })
        .await
    }
}

type Attempt<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(bool, Result<(), Error>), Error>> + Send + 'a>,
>;
async fn reconnect<F>(
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
            result=attempt(&mut config)=>match result {Ok((stable,result))=>{if stable {attempts=0;}result},Err(error)=>Err(error)},
        };
        if cancel.is_cancelled() {
            return Ok(());
        }
        {
            use bridge_app::diagnostics::{Event, Status, emit};
            let status = match &result {
                Ok(()) => Status::Ok,
                Err(Error::Transport) => Status::Transport,
                Err(Error::Protocol) => Status::Protocol,
                Err(Error::Authentication) => Status::Authentication,
                Err(Error::Proxy) => Status::Proxy,
                Err(Error::Overloaded) => Status::Overloaded,
                Err(Error::Exhausted) => Status::Exhausted,
            };
            emit(Event::Reconnect, status, None, attempts as usize);
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
        eprintln!("{{\"event\":\"feishu_native_reconnecting\"}}");
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
        Duration::from_secs(1),
        socket.send(Message::Binary(frame.encode_to_vec().into())),
    )
    .await
    .map_err(|_| Error::Transport)?
    .map_err(|_| Error::Transport)
}
/// Public for deterministic in-memory WebSocket tests; one owner reads and writes the socket.
pub async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    mut socket: WebSocketStream<S>,
    service: i32,
    config: &mut ClientConfig,
    incoming: mpsc::Sender<Received>,
    cancel: CancellationToken,
) -> Result<(), Error> {
    config.validate()?;
    let mut fragments = Fragments::default();
    let mut replies = JoinSet::new();
    let mut last_pong = Instant::now();
    let mut next_ping = Instant::now();
    loop {
        tokio::select! {
            _=cancel.cancelled()=>{let _=timeout(Duration::from_secs(1),socket.close(None)).await;return Ok(());},
            _=tokio::time::sleep_until(next_ping)=>{
                if last_pong.elapsed()>Duration::from_secs(config.ping_interval.saturating_mul(2)+5) {
                    bridge_app::diagnostics::emit(bridge_app::diagnostics::Event::HeartbeatTimeout,bridge_app::diagnostics::Status::Transport,None,0);
                    return Err(Error::Transport);
                }
                write(&mut socket,Frame::ping(service)).await?;next_ping=Instant::now()+Duration::from_secs(config.ping_interval);
            }
            reply=replies.join_next(),if !replies.is_empty()=>{write(&mut socket,reply.ok_or(Error::Protocol)?.map_err(|_|Error::Protocol)?).await?;}
            message=socket.next()=>{
                let message=message.ok_or(Error::Transport)?.map_err(|_|Error::Transport)?;
                let bytes=match message {
                    Message::Binary(bytes)=>bytes,
                    Message::Ping(bytes)=>{timeout(Duration::from_secs(1),socket.send(Message::Pong(bytes))).await.map_err(|_|Error::Transport)?.map_err(|_|Error::Transport)?;continue;},
                    Message::Pong(_)=>continue,
                    Message::Close(_)=>return Err(Error::Transport),
                    _=>return Err(Error::Protocol),
                };
                if bytes.len()>MAX_FRAME_BYTES {return Err(Error::Protocol);}
                let frame=Frame::parse(&bytes)?;
                if frame.service!=Some(service) {return Err(Error::Protocol);}
                let kind=frame.header("type")?;
                if frame.method==Some(0) {
                    if kind=="pong" {
                        last_pong=Instant::now();
                        if let Some(payload)=frame.payload.filter(|p|!p.is_empty()) {
                            config.update(&payload)?;
                            next_ping=Instant::now()+Duration::from_secs(config.ping_interval);
                        }
                    }
                    continue;
                }
                if !matches!(kind,"event"|"card") {continue;}
                let Some(mut frame)=fragments.push(frame,Instant::now())? else {continue;};
                let started=Instant::now();
                let event=wire::event(&frame.payload.take().unwrap_or_default());
                match event {
                    Ok(Some(event))=>{
                        let card=matches!(&event,Event::Card {..});
                        if replies.len()>=64 {write(&mut socket,frame.reply(false,started.elapsed())).await?;continue;}
                        let (receipt,wait)=oneshot::channel();
                        let sent=incoming.try_send(Received {event,acceptance:Some(Acceptance(receipt))}).is_ok();
                        replies.spawn(async move {
                            let accepted=sent && matches!(timeout(Duration::from_secs(1),wait).await,Ok(Ok(true)));
                            frame.reply_with_card(accepted,started.elapsed(),card)
                        });
                    }
                    Ok(None)=>write(&mut socket,frame.reply(true,started.elapsed())).await?,
                    Err(_)=>write(&mut socket,frame.reply(false,started.elapsed())).await?,
                }
            }
        }
    }
}
