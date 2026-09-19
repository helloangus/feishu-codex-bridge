//! Background job completion handling, routed by causal flow.
//!
//! [`Done`] groups completions into four domains and each domain module owns
//! its variants plus the invariants around them:
//!
//! - [`session`] — durable claim chains. Every chain that acquired the
//!   scheduler's session-mutation gate releases it exactly once, at its
//!   terminal event, on every outcome path.
//! - [`task`] — task lifecycle from plan implementation to a running turn.
//! - [`card`] — delivery receipts for approval, panel and list cards.
//! - `delivery` — task-file delivery; its single event resets to idle inline.
mod card;
mod session;
mod task;

use super::RuntimeError;
use super::state::{DeliveryDone, Done, FileDelivery, Runtime};
use tokio::task::JoinSet;

impl Runtime {
    pub(crate) async fn handle_done(
        &mut self,
        done: Done,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match done {
            Done::Session(done) => self.handle_session_done(done, jobs).await,
            Done::Task(done) => self.handle_task_done(done, jobs).await,
            Done::Card(done) => self.handle_card_done(done, jobs).await,
            Done::Delivery(DeliveryDone::FilesDelivered) => {
                self.tasks.files = FileDelivery::Idle;
                Ok(())
            }
        }
    }
}
