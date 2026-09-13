use bridge_feishu::{
    proxy::{Policy, http_connect, socks_connect},
    websocket::Error,
};
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[test]
fn proxy_precedence_explicit_override_and_bypass_boundaries() -> Result {
    let values = [
        ("https_proxy", "http://https:80"),
        ("wss_proxy", "http://wss:80"),
        ("all_proxy", "socks5h://all:1080"),
        ("no_proxy", ".example.com,10.0.0.0/8,[::1]:443,host:invalid"),
    ];
    let get = |name: &str| {
        values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, v)| v.to_string())
    };
    let policy = Policy::from_values(None, get)?;
    for url in [
        "wss://example.com",
        "wss://sub.example.com",
        "wss://10.2.3.4",
        "wss://[::1]",
    ] {
        assert!(policy.select(&Url::parse(url)?).is_none());
    }
    for url in ["wss://notexample.com", "wss://[::1]:444", "wss://host"] {
        assert_eq!(
            policy.select(&Url::parse(url)?).and_then(Url::host_str),
            Some("wss")
        );
    }
    assert_eq!(
        policy
            .select(&Url::parse("https://remote")?)
            .and_then(Url::host_str),
        Some("https")
    );
    let explicit = Policy::from_values(Some("http://override:8080"), get)?;
    assert_eq!(
        explicit
            .select(&Url::parse("wss://example.com")?)
            .and_then(Url::host_str),
        Some("override")
    );
    Ok(())
}

#[tokio::test]
async fn connect_authentication_preserves_tunnel_bytes_and_classifies_failures() -> Result {
    for status in [200, 407, 503] {
        let (mut client, mut server) = tokio::io::duplex(16384);
        let proxy = Url::parse("http://user:p%40ss@proxy:80")?;
        let server = tokio::spawn(async move {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(server.read_u8().await?);
            }
            let request = String::from_utf8(request)?;
            assert!(request.starts_with("CONNECT [::1]:443 HTTP/1.1\r\n"));
            assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwQHNz\r\n"));
            server
                .write_all(format!("HTTP/1.1 {status} result\r\n\r\nX").as_bytes())
                .await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let result = http_connect(&mut client, "::1", 443, &proxy).await;
        match status {
            200 => {
                result?;
                assert_eq!(client.read_u8().await?, b'X');
            }
            407 => assert!(matches!(result, Err(Error::Proxy))),
            _ => assert!(matches!(result, Err(Error::Transport))),
        }
        server.await?.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tokio::test]
async fn socks_authenticated_remote_dns_and_rejection() -> Result {
    for accepted in [true, false] {
        let (mut client, mut server) = tokio::io::duplex(8192);
        let proxy = Url::parse("socks5h://user:pass@proxy:1080")?;
        let server = tokio::spawn(async move {
            let mut greeting = [0; 3];
            server.read_exact(&mut greeting).await?;
            assert_eq!(greeting, [5, 1, 2]);
            server.write_all(&[5, 2]).await?;
            let mut auth = [0; 11];
            server.read_exact(&mut auth).await?;
            assert_eq!(&auth, b"\x01\x04user\x04pass");
            server.write_all(&[1, if accepted { 0 } else { 1 }]).await?;
            if accepted {
                let mut request = [0; 18];
                server.read_exact(&mut request).await?;
                assert_eq!(&request, b"\x05\x01\x00\x03\x0bexample.com\x01\xbb");
                server
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80, 42])
                    .await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let result = socks_connect(&mut client, "example.com", 443, &proxy).await;
        if accepted {
            result?;
            assert_eq!(client.read_u8().await?, 42);
        } else {
            assert!(matches!(result, Err(Error::Proxy)));
        }
        server.await?.map_err(|e| e.to_string())?;
    }
    Ok(())
}
