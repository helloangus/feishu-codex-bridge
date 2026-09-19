//! Admission receipts: one settled decision per inbound input.
//!
//! Every [`Input`](super::Input) carries one `Ack`. The runtime settles it
//! exactly once on every path — accept or reject — and the transport turns the
//! decision into the Feishu event receipt. Dropping an `Ack` without settling
//! rejects the input; an aborted background job therefore can never leave the
//! transport waiting for a receipt that never comes.

/// Exactly-once admission receipt for one input. The first settle wins; a
/// drop without settle rejects.
pub struct Ack(Option<tokio::sync::oneshot::Sender<bool>>);

impl Ack {
    /// A receipt nobody observes; for synthetic inputs in tests.
    pub fn detached() -> Self {
        Self(None)
    }

    /// Wrap the transport-side receipt for one received event.
    pub fn from_receipt(receipt: Option<tokio::sync::oneshot::Sender<bool>>) -> Self {
        Self(receipt)
    }

    /// Settle the admission decision. Later settles are no-ops.
    pub fn settle(&mut self, accepted: bool) {
        if let Some(receipt) = self.0.take() {
            let _ = receipt.send(accepted);
        }
    }
}

impl Drop for Ack {
    fn drop(&mut self) {
        if let Some(receipt) = self.0.take() {
            let _ = receipt.send(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_settle_wins_and_drop_rejects() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut ack = Ack::from_receipt(Some(tx));
        ack.settle(true);
        ack.settle(false);
        assert_eq!(rx.try_recv(), Ok(true));

        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(Ack::from_receipt(Some(tx)));
        assert_eq!(rx.await, Ok(false));

        let mut ack = Ack::detached();
        ack.settle(true);
    }
}
