//! Task-file port fake: a no-op lifecycle for scenarios that never stage files.
use bridge_app::files::{Attachment, PreparedFiles, TaskFiles};
use std::path::PathBuf;

/// No-op [`TaskFiles`]: staging accepts nothing, binding and finishing succeed.
#[derive(Debug, Default, Clone, Copy)]
pub struct IdleFiles;

impl TaskFiles for IdleFiles {
    fn bind_files(&self, _: String, _: String) -> bridge_app::messaging::DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn prepare_files(
        &self,
        _: String,
        _: PathBuf,
        _: Vec<Attachment>,
    ) -> bridge_app::messaging::DeliveryFuture<'_, PreparedFiles> {
        Box::pin(async { Ok(PreparedFiles::default()) })
    }
    fn finish_files(
        &self,
        _: String,
        _: String,
        _: PathBuf,
    ) -> bridge_app::messaging::DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
