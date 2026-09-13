//! Explicit proxy selection and bounded HTTP CONNECT/SOCKS5 tunnels for native TLS WebSockets.
use crate::websocket::Error;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Url;
use std::{net::IpAddr, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, rustls};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream,
    tungstenite::{self, protocol::WebSocketConfig},
};

// No Debug: URLs can contain proxy credentials or long-connection authentication.
pub struct Policy {
    explicit: Option<Url>,
    https: Option<Url>,
    http: Option<Url>,
    wss: Option<Url>,
    all: Option<Url>,
    bypass: String,
}
impl Policy {
    pub fn from_env(explicit: Option<&str>) -> Result<Self, Error> {
        Self::from_values(explicit, |name| std::env::var(name).ok())
    }
    pub fn from_values(
        explicit: Option<&str>,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, Error> {
        fn parse(value: Option<String>) -> Result<Option<Url>, Error> {
            value
                .filter(|v| !v.is_empty())
                .map(|value| {
                    let url = Url::parse(&value).map_err(|_| Error::Proxy)?;
                    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                        || url.host_str().is_none()
                        || url.query().is_some()
                        || url.fragment().is_some()
                        || url.path() != "/" && !url.path().is_empty()
                    {
                        return Err(Error::Proxy);
                    }
                    Ok(url)
                })
                .transpose()
        }
        if let Some(explicit) = explicit.filter(|v| !v.is_empty()) {
            return Ok(Self {
                explicit: parse(Some(explicit.into()))?,
                https: None,
                http: None,
                wss: None,
                all: None,
                bypass: String::new(),
            });
        }
        let value =
            |lower: &str, upper: &str| get(lower).filter(|v| !v.is_empty()).or_else(|| get(upper));
        Ok(Self {
            explicit: None,
            https: parse(value("https_proxy", "HTTPS_PROXY"))?,
            http: parse(value("http_proxy", "HTTP_PROXY"))?,
            wss: parse(value("wss_proxy", "WSS_PROXY"))?,
            all: parse(value("all_proxy", "ALL_PROXY"))?,
            bypass: value("no_proxy", "NO_PROXY").unwrap_or_default(),
        })
    }
    pub fn select(&self, target: &Url) -> Option<&Url> {
        if self.explicit.is_some() {
            return self.explicit.as_ref();
        }
        if bypass(&self.bypass, target) {
            return None;
        }
        match target.scheme() {
            "wss" => self.wss.as_ref().or(self.https.as_ref()),
            "https" => self.https.as_ref(),
            _ => self.http.as_ref(),
        }
        .or(self.all.as_ref())
    }
}
fn host(url: &Url) -> Result<&str, Error> {
    url.host_str()
        .map(|h| h.trim_matches(['[', ']']))
        .ok_or(Error::Proxy)
}
fn bypass(rules: &str, target: &Url) -> bool {
    let Ok(host) = host(target) else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    rules
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|rule| {
            if rule == "*" {
                return true;
            }
            if let (Ok(net), Ok(ip)) = (rule.parse::<ipnet::IpNet>(), host.parse::<IpAddr>()) {
                return net.contains(&ip);
            }
            let rule = rule.to_ascii_lowercase();
            let (domain, port) = if let Some(end) = rule.find(']').filter(|_| rule.starts_with('['))
            {
                let suffix = &rule[end + 1..];
                let port = if suffix.is_empty() {
                    None
                } else if let Some(port) =
                    suffix.strip_prefix(':').and_then(|p| p.parse::<u16>().ok())
                {
                    Some(port)
                } else {
                    return false;
                };
                (&rule[1..end], port)
            } else if rule.matches(':').count() == 1 {
                if rule
                    .rsplit_once(':')
                    .is_some_and(|(_, p)| p.parse::<u16>().is_err())
                {
                    return false;
                }
                rule.rsplit_once(':')
                    .map(|(h, p)| (h, p.parse::<u16>().ok()))
                    .unwrap_or((&rule, None))
            } else {
                (&*rule, None)
            };
            if port.is_some() && port != target.port_or_known_default() {
                return false;
            }
            let domain = domain.trim_start_matches('.').trim_matches(['[', ']']);
            host == domain || host.ends_with(&format!(".{domain}"))
        })
}
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;
fn tls_config() -> Result<Arc<rustls::ClientConfig>, Error> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Transport)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}
fn credential(url: &Url) -> Result<(String, String), Error> {
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8()
            .map(|s| s.into_owned())
            .map_err(|_| Error::Proxy)
    };
    Ok((
        decode(url.username())?,
        decode(url.password().unwrap_or(""))?,
    ))
}
/// The caller bounds total dial/handshake time and owns cancellation.
pub async fn connect(
    url: &Url,
    policy: &Policy,
) -> Result<WebSocketStream<MaybeTlsStream<Stream>>, Error> {
    let target = host(url)?;
    let port = url.port_or_known_default().ok_or(Error::Proxy)?;
    let tls = tls_config()?;
    let stream: Stream = if let Some(proxy) = policy.select(url) {
        let proxy_host = host(proxy)?;
        let proxy_port = proxy.port_or_known_default().unwrap_or(1080);
        let tcp = TcpStream::connect((proxy_host, proxy_port))
            .await
            .map_err(|_| Error::Transport)?;
        let mut stream: Stream = if proxy.scheme() == "https" {
            let name = rustls::pki_types::ServerName::try_from(proxy_host.to_owned())
                .map_err(|_| Error::Proxy)?;
            Box::new(
                TlsConnector::from(tls.clone())
                    .connect(name, tcp)
                    .await
                    .map_err(|_| Error::Transport)?,
            )
        } else {
            Box::new(tcp)
        };
        match proxy.scheme() {
            "http" | "https" => http_connect(&mut stream, target, port, proxy).await?,
            "socks5" | "socks5h" => {
                let destination = if proxy.scheme() == "socks5" && target.parse::<IpAddr>().is_err()
                {
                    tokio::net::lookup_host((target, port))
                        .await
                        .map_err(|_| Error::Transport)?
                        .next()
                        .ok_or(Error::Transport)?
                        .ip()
                        .to_string()
                } else {
                    target.to_owned()
                };
                socks_connect(&mut stream, &destination, port, proxy).await?;
            }
            _ => return Err(Error::Proxy),
        }
        stream
    } else {
        Box::new(
            TcpStream::connect((target, port))
                .await
                .map_err(|_| Error::Transport)?,
        )
    };
    handshake(url, stream, tls).await
}
async fn handshake(
    url: &Url,
    stream: Stream,
    tls: Arc<rustls::ClientConfig>,
) -> Result<WebSocketStream<MaybeTlsStream<Stream>>, Error> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(crate::ingress::MAX_FRAME_BYTES))
        .max_frame_size(Some(crate::ingress::MAX_FRAME_BYTES));
    tokio_tungstenite::client_async_tls_with_config(
        url.as_str(),
        stream,
        Some(config),
        Some(Connector::Rustls(tls)),
    )
    .await
    .map(|(socket, _)| socket)
    .map_err(|error| {
        if let tungstenite::Error::Http(response) = error {
            let header = |key: &str| response.headers().get(key).and_then(|h| h.to_str().ok());
            if response.status().as_u16() == 403
                || header("handshake-status") == Some("403")
                || header("handshake-autherrcode") == Some("1000040350")
            {
                return Error::Authentication;
            }
        }
        Error::Transport
    })
}

#[cfg(test)]
#[path = "proxy_tls_tests.rs"]
mod tls_tests;
pub async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
    proxy: &Url,
) -> Result<(), Error> {
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    if authority.chars().any(|c| c.is_control()) {
        return Err(Error::Proxy);
    }
    let (user, password) = credential(proxy)?;
    let auth = if user.is_empty() && password.is_empty() {
        String::new()
    } else {
        format!(
            "Proxy-Authorization: Basic {}\r\n",
            STANDARD.encode(format!("{user}:{password}"))
        )
    };
    if auth.len() > 4096 {
        return Err(Error::Proxy);
    }
    stream
        .write_all(
            format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{auth}\r\n").as_bytes(),
        )
        .await
        .map_err(|_| Error::Transport)?;
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() >= 8192 {
            return Err(Error::Proxy);
        }
        response.push(stream.read_u8().await.map_err(|_| Error::Transport)?);
    }
    let status = std::str::from_utf8(&response)
        .map_err(|_| Error::Proxy)?
        .lines()
        .next()
        .ok_or(Error::Proxy)?;
    let parts: Vec<_> = status.split_whitespace().collect();
    if parts.len() >= 2 && parts[1].parse::<u16>().is_ok_and(|code| code >= 500) {
        return Err(Error::Transport);
    }
    if parts.len() < 2 || !matches!(parts[0], "HTTP/1.1" | "HTTP/1.0") || parts[1] != "200" {
        return Err(Error::Proxy);
    }
    Ok(())
}
pub async fn socks_connect<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
    proxy: &Url,
) -> Result<(), Error> {
    let (user, password) = credential(proxy)?;
    let auth = !user.is_empty() || !password.is_empty();
    stream
        .write_all(if auth { &[5, 1, 2] } else { &[5, 1, 0] })
        .await
        .map_err(|_| Error::Transport)?;
    let mut method = [0; 2];
    stream
        .read_exact(&mut method)
        .await
        .map_err(|_| Error::Transport)?;
    if method != [5, if auth { 2 } else { 0 }] {
        return Err(Error::Proxy);
    }
    if auth {
        if user.is_empty() || user.len() > 255 || password.len() > 255 {
            return Err(Error::Proxy);
        }
        let mut bytes = vec![1, user.len() as u8];
        bytes.extend_from_slice(user.as_bytes());
        bytes.push(password.len() as u8);
        bytes.extend_from_slice(password.as_bytes());
        stream
            .write_all(&bytes)
            .await
            .map_err(|_| Error::Transport)?;
        stream
            .read_exact(&mut method)
            .await
            .map_err(|_| Error::Transport)?;
        if method != [1, 0] {
            return Err(Error::Proxy);
        }
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.is_empty() || host.len() > 255 {
                return Err(Error::Proxy);
            }
            request.extend_from_slice(&[3, host.len() as u8]);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream
        .write_all(&request)
        .await
        .map_err(|_| Error::Transport)?;
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| Error::Transport)?;
    if header[..3] != [5, 0, 0] {
        return Err(Error::Proxy);
    }
    let length = match header[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await.map_err(|_| Error::Transport)?),
        _ => return Err(Error::Proxy),
    };
    let mut address = vec![0; length + 2];
    stream
        .read_exact(&mut address)
        .await
        .map_err(|_| Error::Transport)?;
    Ok(())
}
