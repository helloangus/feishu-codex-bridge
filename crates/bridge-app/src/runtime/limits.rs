//! Fixed runtime bounds. These remain internal so configuration and persisted
//! state stay compatible while every producer applies the same limits.
use std::time::Duration;

pub(crate) const BACKGROUND_JOBS: usize = 128;
pub(crate) const CONTROL_JOB_RESERVE: usize = 16;
pub(crate) const DELIVERY_QUEUE: usize = 128;
pub(crate) const DELIVERY_PROGRESS_RESERVE: usize = 8;
pub(crate) const INTERACTIONS: usize = 32;
pub(crate) const QUESTIONS_PER_REQUEST: usize = 32;
pub(crate) const FILE_CHANGE_ITEMS: usize = 32;
pub(crate) const ARCHIVED_THREADS: usize = 128;
pub(crate) const PANEL_REFRESHES: usize = 128;
pub(crate) const EARLY_PROTOCOL_EVENTS: usize = 64;
pub(crate) const SCHEDULED_TASKS: usize = 64;
pub(crate) const SEEN_COMMANDS: usize = 1_000;
pub(crate) const ATTACHMENTS_PER_MESSAGE: usize = 10;
pub(crate) const INPUT_BYTES: usize = 32 * 1024;
pub(crate) const ANSWER_BYTES: usize = 16 * 1024;
pub(crate) const PATH_BYTES: usize = 4_096;
pub(crate) const PAIRING_CODE_BYTES: usize = 256;
pub(crate) const PAIRING_ATTEMPTS: u32 = 10;
pub(crate) const PLAN_BYTES: usize = 16_000;
pub(crate) const OUTPUT_BYTES: usize = 32 * 1024;
pub(crate) const PREVIEW_CHARS: usize = 1_000;

pub(crate) const TICK: Duration = Duration::from_secs(1);
pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_secs(3);
pub(crate) const TASK_TIMEOUT: Duration = Duration::from_secs(3_600);
pub(crate) const INTERACTION_TIMEOUT: Duration = Duration::from_secs(600);
pub(crate) const PAIRING_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const BACKEND_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MESSAGE_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const ANSWER_DELIVERY_TIMEOUT: Duration = Duration::from_secs(180);
pub(crate) const FILE_PREPARE_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const FILE_BIND_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const FILE_FINISH_TIMEOUT: Duration = Duration::from_secs(180);
pub(crate) const ATTACHMENT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const DIFF_PANEL_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const SHUTDOWN_INTERRUPT_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const SHUTDOWN_REPLY_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const SHUTDOWN_SENDER_TIMEOUT: Duration = Duration::from_secs(5);
