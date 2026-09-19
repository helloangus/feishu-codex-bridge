//! Runtime `Input` construction shared by behavior tests.
//!
//! Every caller used to re-declare the acknowledgement plumbing; these helpers
//! keep one canonical construction and return the admission result receiver.

use bridge_app::{cards::Click, files::Attachment, runtime, runtime::Input};
use std::error::Error;
use tokio::sync::{mpsc, oneshot};

/// Build one input plus the receiver of its admission acknowledgement.
pub fn make_input(
    id: &str,
    user: &str,
    chat: &str,
    text: Option<String>,
    card: Option<Click>,
    attachments: Vec<Attachment>,
) -> (Input, oneshot::Receiver<bool>) {
    let (ack, wait) = oneshot::channel();
    (
        Input {
            attachments,
            card,
            id: id.to_owned(),
            user: user.to_owned(),
            chat: chat.to_owned(),
            text,
            ack: runtime::Ack::from_receipt(Some(ack)),
        },
        wait,
    )
}

/// Send one input and wait for the runtime's admission decision.
pub async fn submit(
    tx: &mpsc::Sender<Input>,
    id: &str,
    user: &str,
    chat: &str,
    text: Option<String>,
    card: Option<Click>,
    attachments: Vec<Attachment>,
) -> Result<bool, Box<dyn Error>> {
    let (input, wait) = make_input(id, user, chat, text, card, attachments);
    tx.send(input).await.map_err(|_| "input channel closed")?;
    Ok(wait.await?)
}

/// Send a plain text message as `chat` and require admission.
pub async fn submit_text(
    tx: &mpsc::Sender<Input>,
    id: &str,
    user: &str,
    chat: &str,
    text: &str,
) -> Result<(), Box<dyn Error>> {
    let accepted = submit(tx, id, user, chat, Some(text.to_owned()), None, Vec::new()).await?;
    assert!(accepted, "input '{id}' was not admitted");
    Ok(())
}
