//! Recording port fake for the `Messenger` boundary.
//!
//! Deliveries are captured in bounded channels so tests can await concrete
//! texts, cards and updates; failure switches reproduce the transport-failure
//! and one-way-update behaviours the production delivery paths must survive.

use bridge_app::messaging::{DeliveryError, DeliveryFuture, MessageId, Messenger, ResourceKind};
use bridge_core::view::Panel;
use std::{
    error::Error,
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

/// Channel capacity used for every recording channel.
const CAPACITY: usize = 64;

/// Static failure behaviour configured before the harness starts.
#[derive(Clone, Copy, Debug)]
pub struct MessengerOptions {
    /// `send_panel` fails with a transport error.
    pub panels_fail: bool,
    /// `update_panel` records the update first, then fails.
    pub updates_fail: bool,
    /// `send_text` fails with a transport error.
    pub texts_fail: bool,
    /// `upload` panics; use only where no upload may ever happen.
    pub upload_panics: bool,
}

impl MessengerOptions {
    /// Deliver everything successfully and reject uploads with an error.
    pub fn new() -> Self {
        Self {
            panels_fail: false,
            updates_fail: false,
            texts_fail: false,
            upload_panics: false,
        }
    }
}

impl Default for MessengerOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Receivers of everything the runtime delivered through the fake.
pub struct MessengerHandles {
    pub text: mpsc::Receiver<(String, String)>,
    pub panels: mpsc::Receiver<(String, Panel)>,
    pub updates: mpsc::Receiver<(String, Panel)>,
}

struct Recordings {
    prefix: &'static str,
    text: mpsc::Sender<(String, String)>,
    panels: mpsc::Sender<(String, Panel)>,
    updates: mpsc::Sender<(String, Panel)>,
    panels_fail: AtomicBool,
    updates_fail: AtomicBool,
    texts_fail: AtomicBool,
    upload_panics: AtomicBool,
    sequence: AtomicU64,
}

/// [`Messenger`] port fake shared between the runtime and the test assertions.
#[derive(Clone)]
pub struct RecordingMessenger(Arc<Recordings>);

impl RecordingMessenger {
    /// Create the fake plus its receivers.
    pub fn recorded(
        prefix: &'static str,
        options: MessengerOptions,
    ) -> (Arc<Self>, MessengerHandles) {
        let (text_tx, text) = mpsc::channel(CAPACITY);
        let (panels_tx, panels) = mpsc::channel(CAPACITY);
        let (updates_tx, updates) = mpsc::channel(CAPACITY);
        let recordings = Recordings {
            prefix,
            text: text_tx,
            panels: panels_tx,
            updates: updates_tx,
            panels_fail: AtomicBool::new(options.panels_fail),
            updates_fail: AtomicBool::new(options.updates_fail),
            texts_fail: AtomicBool::new(options.texts_fail),
            upload_panics: AtomicBool::new(options.upload_panics),
            sequence: AtomicU64::new(1),
        };
        (
            Self(Arc::new(recordings)).shared(),
            MessengerHandles {
                text,
                panels,
                updates,
            },
        )
    }

    /// Wrap this fake in an `Arc` so it can be handed to the runtime.
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Flip the `send_panel` failure switch at runtime.
    pub fn set_panels_failed(&self, failed: bool) {
        self.0.panels_fail.store(failed, Ordering::Relaxed);
    }

    /// Flip the `update_panel` failure switch at runtime.
    pub fn set_updates_failed(&self, failed: bool) {
        self.0.updates_fail.store(failed, Ordering::Relaxed);
    }
}

impl Messenger for RecordingMessenger {
    fn send_text(&self, chat: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            if self.0.texts_fail.load(Ordering::Relaxed) {
                return Err(DeliveryError::Transport);
            }
            self.0
                .text
                .send((chat, text))
                .await
                .map_err(|_| DeliveryError::Transport)
        })
    }

    fn send_panel(&self, _: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async move {
            if self.0.panels_fail.load(Ordering::Relaxed) {
                return Err(DeliveryError::Transport);
            }
            let id = format!(
                "{}-{}",
                self.0.prefix,
                self.0.sequence.fetch_add(1, Ordering::Relaxed)
            );
            self.0
                .panels
                .send((id.clone(), panel))
                .await
                .map_err(|_| DeliveryError::Transport)?;
            Ok(MessageId(id))
        })
    }

    fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.0
                .updates
                .send((id.0, panel))
                .await
                .map_err(|_| DeliveryError::Transport)?;
            // A failed visual update must not replay or undo the command.
            if self.0.updates_fail.load(Ordering::Relaxed) {
                Err(DeliveryError::Transport)
            } else {
                Ok(())
            }
        })
    }

    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            if self.0.upload_panics.load(Ordering::Relaxed) {
                panic!("unexpected upload");
            }
            Err(DeliveryError::Transport)
        })
    }
}

/// Await the next delivered text containing `pattern` (any chat).
pub async fn expect_text(
    handles: &mut MessengerHandles,
    pattern: &str,
) -> Result<(String, String), Box<dyn Error>> {
    loop {
        let (chat, text) = handles.text.recv().await.ok_or("delivery closed")?;
        if text.contains(pattern) {
            return Ok((chat, text));
        }
    }
}

/// Like [`expect_text`], but asserts the delivery chat on every message.
pub async fn expect_chat_text(
    handles: &mut MessengerHandles,
    chat: &str,
    pattern: &str,
) -> Result<String, Box<dyn Error>> {
    loop {
        let (delivered, text) = handles.text.recv().await.ok_or("delivery closed")?;
        assert_eq!(delivered, chat);
        if text.contains(pattern) {
            return Ok(text);
        }
    }
}

/// [`expect_text`] bounded by a timeout that names the missed pattern.
pub async fn expect_text_within(
    handles: &mut MessengerHandles,
    limit: Duration,
    pattern: &str,
) -> Result<(String, String), Box<dyn Error>> {
    let (chat, text) = tokio::time::timeout(limit, expect_text(handles, pattern))
        .await
        .map_err(|_| format!("waiting for text: {pattern}"))??;
    Ok((chat, text))
}

/// Await the next delivered card within the shared assertion timeout.
pub async fn expect_card(
    handles: &mut MessengerHandles,
) -> Result<(String, Panel), Box<dyn Error>> {
    let card = tokio::time::timeout(Duration::from_secs(3), handles.panels.recv())
        .await
        .map_err(|_| "waiting for card")?
        .ok_or("missing card")?;
    Ok(card)
}

/// Await the next card update.
pub async fn expect_update(
    handles: &mut MessengerHandles,
) -> Result<(String, Panel), Box<dyn Error>> {
    handles
        .updates
        .recv()
        .await
        .ok_or_else(|| "missing update".into())
}
