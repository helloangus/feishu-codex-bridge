use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn certificates() -> Result<(Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>)> {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert = certificate.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.key_pair.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert)?;
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((Arc::new(client), Arc::new(server)))
}

#[tokio::test]
async fn secure_websocket_over_direct_http_and_https_tunnels() -> Result {
    for mode in ["direct", "http", "https"] {
        let (tls, server_config) = certificates()?;
        let (client, server) = tokio::io::duplex(65536);
        let server = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(server_config);
            let mut stream: Stream = if mode == "https" {
                Box::new(acceptor.accept(server).await?)
            } else {
                Box::new(server)
            };
            if mode != "direct" {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await?);
                }
                assert!(request.starts_with(b"CONNECT localhost:443 HTTP/1.1"));
                stream
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await?;
            }
            let stream = acceptor.accept(stream).await?;
            let mut socket = tokio_tungstenite::accept_async(stream).await?;
            assert_eq!(
                socket.next().await.ok_or("closed")??,
                Message::Text("hello".into())
            );
            socket.send(Message::Text("world".into())).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let mut stream: Stream = if mode == "https" {
            Box::new(
                TlsConnector::from(tls.clone())
                    .connect(
                        rustls::pki_types::ServerName::try_from("localhost")?,
                        client,
                    )
                    .await?,
            )
        } else {
            Box::new(client)
        };
        if mode != "direct" {
            http_connect(&mut stream, "localhost", 443, &Url::parse("http://proxy")?).await?;
        }
        let mut socket = handshake(&Url::parse("wss://localhost/ws")?, stream, tls).await?;
        socket.send(Message::Text("hello".into())).await?;
        assert_eq!(
            socket.next().await.ok_or("closed")??,
            Message::Text("world".into())
        );
        server.await?.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tokio::test]
async fn tls_rejects_wrong_hostname_and_untrusted_server() -> Result {
    for wrong_host in [true, false] {
        let (trusted, server_config) = certificates()?;
        let (client, server) = tokio::io::duplex(65536);
        let peer =
            tokio::spawn(async move { TlsAcceptor::from(server_config).accept(server).await });
        let tls = if wrong_host { trusted } else { tls_config()? };
        let url = Url::parse(if wrong_host {
            "wss://wrong.invalid/ws"
        } else {
            "wss://localhost/ws"
        })?;
        assert!(handshake(&url, Box::new(client), tls).await.is_err());
        let _ = peer.await?;
    }
    Ok(())
}
