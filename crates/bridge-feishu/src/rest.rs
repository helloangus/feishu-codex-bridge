//! Feishu REST implementation with shared authentication and bounded transfers.
use crate::cards;
use bridge_app::messaging::{
    DeliveryError, DeliveryFuture, MessageId, Messenger, ResourceFetcher, ResourceKind, ResourceRef,
};
use bridge_core::view::Panel;
use futures_util::StreamExt;
use reqwest::{Client, Method, Url, multipart};
use serde_json::{Value, json};
use std::{
    fs::File,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore},
};
use tokio_util::io::ReaderStream;

struct Token {
    value: String,
    expires: Instant,
}

/// Deliberately does not implement Debug: credentials must never be logged.
pub struct FeishuRest {
    http: Client,
    base: Url,
    app_id: String,
    app_secret: String,
    token: Mutex<Option<Token>>,
    requests: Semaphore,
    transfers: Semaphore,
    max_attachment: u64,
}

impl FeishuRest {
    pub fn new(
        app_id: String,
        app_secret: String,
        proxy: Option<&str>,
        max_attachment: u64,
    ) -> Result<Self, DeliveryError> {
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10));
        if let Some(proxy) = proxy {
            builder = builder
                .no_proxy()
                .proxy(reqwest::Proxy::all(proxy).map_err(|_| DeliveryError::Transport)?);
        }
        let http = builder.build().map_err(|_| DeliveryError::Transport)?;
        Self::with_client(
            http,
            Url::parse("https://open.feishu.cn/open-apis/")
                .map_err(|_| DeliveryError::Incompatible)?,
            app_id,
            app_secret,
            max_attachment,
        )
    }

    /// Alternate endpoint/client is injectable for loopback contract tests.
    pub fn with_client(
        http: Client,
        base: Url,
        app_id: String,
        app_secret: String,
        max_attachment: u64,
    ) -> Result<Self, DeliveryError> {
        if app_id.is_empty() || app_secret.is_empty() {
            return Err(DeliveryError::Authentication);
        }
        Ok(Self {
            http,
            base,
            app_id,
            app_secret,
            token: Mutex::new(None),
            requests: Semaphore::new(4),
            transfers: Semaphore::new(2),
            max_attachment,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, DeliveryError> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| DeliveryError::Incompatible)?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    async fn token(&self) -> Result<String, DeliveryError> {
        // A single refresh owner. This mutex protects only authentication,
        // never application/session state or normal response delivery.
        let mut cached = self.token.lock().await;
        if let Some(token) = &*cached {
            if token.expires > Instant::now() {
                return Ok(token.value.clone());
            }
        }
        let response = self
            .http
            .post(self.endpoint(&["auth", "v3", "tenant_access_token", "internal"])?)
            .json(&json!({"app_id":self.app_id,"app_secret":self.app_secret}))
            .send()
            .await
            .map_err(|_| DeliveryError::Transport)?;
        let data = checked(response).await?;
        let value = data
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .ok_or(DeliveryError::Authentication)?
            .to_owned();
        let expiry = data
            .get("expire")
            .and_then(Value::as_u64)
            .unwrap_or(7200)
            .min(86_400);
        *cached = Some(Token {
            value: value.clone(),
            expires: Instant::now() + Duration::from_secs(expiry.saturating_sub(120)),
        });
        Ok(value)
    }

    async fn request(&self, method: Method, url: Url, body: Value) -> Result<Value, DeliveryError> {
        let _permit = self
            .requests
            .acquire()
            .await
            .map_err(|_| DeliveryError::Transport)?;
        let token = self.token().await?;
        let response = self
            .http
            .request(method, url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|_| DeliveryError::Transport)?;
        checked(response).await
    }

    async fn message(
        &self,
        chat: String,
        kind: &str,
        content: Value,
    ) -> Result<MessageId, DeliveryError> {
        let mut url = self.endpoint(&["im", "v1", "messages"])?;
        url.query_pairs_mut()
            .append_pair("receive_id_type", "chat_id");
        let response = self
            .request(
                Method::POST,
                url,
                json!({"receive_id":chat,"msg_type":kind,"content":content.to_string()}),
            )
            .await?;
        let id = response
            .get("data")
            .and_then(|data| data.get("message_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(DeliveryError::Incompatible)?;
        Ok(MessageId(id.into()))
    }
}

async fn checked(response: reqwest::Response) -> Result<Value, DeliveryError> {
    if !response.status().is_success() {
        return Err(DeliveryError::Rejected(i64::from(
            response.status().as_u16(),
        )));
    }
    // API JSON is bounded separately from attachment streams.
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| DeliveryError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
            return Err(DeliveryError::Incompatible);
        }
        bytes.extend_from_slice(&chunk);
    }
    let data: Value = serde_json::from_slice(&bytes).map_err(|_| DeliveryError::Incompatible)?;
    let code = data
        .get("code")
        .and_then(Value::as_i64)
        .ok_or(DeliveryError::Incompatible)?;
    if code != 0 {
        return Err(DeliveryError::Rejected(code));
    }
    Ok(data)
}

impl Messenger for FeishuRest {
    fn send_panel(&self, chat: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async move {
            self.message(chat, "interactive", cards::render(&panel))
                .await
        })
    }
    fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.request(
                Method::PATCH,
                self.endpoint(&["im", "v1", "messages", &id.0])?,
                json!({"msg_type":"interactive","content":cards::render(&panel).to_string()}),
            )
            .await?;
            Ok(())
        })
    }
    fn send_text(&self, chat: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            let text = if text.is_empty() {
                "（无文本回复）".into()
            } else {
                text
            };
            let chars: Vec<_> = text.chars().collect();
            for chunk in chars.chunks(3500) {
                self.message(
                    chat.clone(),
                    "text",
                    json!({"text":chunk.iter().collect::<String>()}),
                )
                .await?;
            }
            Ok(())
        })
    }
    fn upload(
        &self,
        chat: String,
        name: String,
        file: File,
        kind: ResourceKind,
    ) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            let _permit = self
                .transfers
                .acquire()
                .await
                .map_err(|_| DeliveryError::Transport)?;
            let metadata = file.metadata().map_err(|_| DeliveryError::LocalIo)?;
            if !metadata.is_file() {
                return Err(DeliveryError::LocalIo);
            }
            if metadata.len() > self.max_attachment {
                return Err(DeliveryError::TooLarge);
            }
            let size = metadata.len();
            let file = tokio::fs::File::from_std(file);
            let reader = tokio::io::AsyncReadExt::take(file, size);
            let part = multipart::Part::stream_with_length(
                reqwest::Body::wrap_stream(ReaderStream::new(reader)),
                size,
            )
            .file_name(name.clone());
            let (endpoint, field, mut form) = match kind {
                ResourceKind::Image => (
                    "images",
                    "image",
                    multipart::Form::new().text("image_type", "message"),
                ),
                ResourceKind::File => (
                    "files",
                    "file",
                    multipart::Form::new()
                        .text("file_type", "stream")
                        .text("file_name", name),
                ),
            };
            form = form.part(field, part);
            let response = self
                .http
                .post(self.endpoint(&["im", "v1", endpoint])?)
                .bearer_auth(self.token().await?)
                .multipart(form)
                .send()
                .await
                .map_err(|_| DeliveryError::Transport)?;
            let data = checked(response).await?;
            let key_field = if kind == ResourceKind::Image {
                "image_key"
            } else {
                "file_key"
            };
            let key = data
                .get("data")
                .and_then(|data| data.get(key_field))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or(DeliveryError::Incompatible)?;
            self.message(chat, field, json!({key_field:key})).await?;
            Ok(())
        })
    }
}

impl ResourceFetcher for FeishuRest {
    fn download(&self, resource: ResourceRef, destination: File) -> DeliveryFuture<'_, u64> {
        Box::pin(async move {
            let _permit = self
                .transfers
                .acquire()
                .await
                .map_err(|_| DeliveryError::Transport)?;
            let mut url = self.endpoint(&[
                "im",
                "v1",
                "messages",
                &resource.message_id,
                "resources",
                &resource.key,
            ])?;
            url.query_pairs_mut().append_pair(
                "type",
                if resource.kind == ResourceKind::Image {
                    "image"
                } else {
                    "file"
                },
            );
            let response = self
                .http
                .get(url)
                .bearer_auth(self.token().await?)
                .send()
                .await
                .map_err(|_| DeliveryError::Transport)?;
            if !response.status().is_success() {
                return Err(DeliveryError::Rejected(i64::from(
                    response.status().as_u16(),
                )));
            }
            if response
                .content_length()
                .is_some_and(|size| size > self.max_attachment)
            {
                return Err(DeliveryError::TooLarge);
            }
            let mut file = tokio::fs::File::from_std(destination);
            let mut size = 0_u64;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| DeliveryError::Transport)?;
                size = size.saturating_add(chunk.len() as u64);
                if size > self.max_attachment {
                    return Err(DeliveryError::TooLarge);
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|_| DeliveryError::LocalIo)?;
            }
            file.sync_all().await.map_err(|_| DeliveryError::LocalIo)?;
            Ok(size)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    fn response(status: u16, body: &str) -> Result<reqwest::Response, Box<dyn Error>> {
        Ok(http::Response::builder()
            .status(status)
            .body(body.to_owned())?
            .into())
    }

    #[tokio::test]
    async fn http_success_does_not_hide_business_rejection() -> Result<(), Box<dyn Error>> {
        assert_eq!(
            checked(response(
                200,
                r#"{"code":99991663,"msg":"private token detail"}"#
            )?)
            .await,
            Err(DeliveryError::Rejected(99991663))
        );
        assert_eq!(
            checked(response(429, "private rate limit detail")?).await,
            Err(DeliveryError::Rejected(429))
        );
        assert_eq!(
            checked(response(200, r#"{"code":0,"data":{"message_id":"m"}}"#)?).await?,
            json!({"code":0,"data":{"message_id":"m"}})
        );
        Ok(())
    }

    #[tokio::test]
    async fn malformed_missing_and_oversized_responses_are_rejected() -> Result<(), Box<dyn Error>>
    {
        for body in ["not JSON", r#"{"data":{}}"#, r#"{"code":"0"}"#] {
            assert_eq!(
                checked(response(200, body)?).await,
                Err(DeliveryError::Incompatible)
            );
        }
        assert_eq!(
            checked(response(200, &" ".repeat(2 * 1024 * 1024 + 1))?).await,
            Err(DeliveryError::Incompatible)
        );
        Ok(())
    }

    #[test]
    fn identifiers_cannot_inject_endpoint_segments_or_queries() -> Result<(), Box<dyn Error>> {
        let rest = FeishuRest::with_client(
            Client::builder().no_proxy().build()?,
            Url::parse("https://example.invalid/open-apis/")?,
            "fake-id".into(),
            "fake-secret".into(),
            1024,
        )?;
        let url = rest.endpoint(&["im", "v1", "messages", "a/b?x=1#frag"])?;
        assert_eq!(url.path(), "/open-apis/im/v1/messages/a%2Fb%3Fx=1%23frag");
        assert!(url.query().is_none());
        assert!(url.fragment().is_none());
        Ok(())
    }
}
