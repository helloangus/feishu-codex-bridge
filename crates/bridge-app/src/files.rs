//! Task-file port and delivery strategy: attachment staging, artifact
//! selection, upload ordering and failure feedback. Filesystem mechanics stay
//! behind [`LocalFiles`]; transport stays behind [`Messenger`].
use crate::{
    diagnostics::{self, Diagnostics},
    messaging::{DeliveryError, DeliveryFuture, Messenger, ResourceKind, ResourceRef},
    runtime::limits,
};
use bridge_core::view::{Panel, Tone};
use std::{
    collections::BTreeMap,
    fs::File,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::time::timeout;

/// Classification of one scanned workspace file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Ignore,
    Text,
    Artifact,
}

/// Bounded snapshot and diff budgets; producers apply the same limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub files: usize,
    pub text_bytes: usize,
    pub entries: usize,
    pub diff_chars: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            files: 200,
            text_bytes: 256 * 1024,
            entries: 10_000,
            diff_chars: 20_000,
        }
    }
}

/// One file entry of a workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: FileKind,
    pub bytes: u64,
    pub modified: Option<std::time::SystemTime>,
    pub text: Option<String>,
    pub skipped: Option<String>,
    pub digest: Option<[u8; 32]>,
}

/// Point-in-time view of a workspace; only metadata and bounded text.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub files: BTreeMap<PathBuf, Entry>,
    pub complete: bool,
}

/// Rendered bounded diff of one text file.
#[derive(Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: PathBuf,
    pub content: String,
}

/// Filesystem primitives the strategy composes; blocking and object-safe so
/// the adapter can supply safe opens, snapshots, diffs and stable handles.
/// One attachment staging request inside a workspace.
pub struct StageRequest<'a> {
    pub directory: PathBuf,
    pub task: &'a str,
    pub index: usize,
    pub name: &'a str,
    /// Image slots verify the magic bytes before publication.
    pub image: bool,
    pub max_bytes: u64,
}

/// One changed file selected for upload, with the tree it was found under.
struct Candidate {
    root: PathBuf,
    relative: PathBuf,
    bytes: u64,
    digest: Option<[u8; 32]>,
}

/// Diffs and upload candidates collected for one finished task.
struct Collected {
    diffs: Vec<FileDiff>,
    candidates: Vec<Candidate>,
    complete: bool,
}

pub trait LocalFiles: Send + Sync {
    /// Bounded scan of a safely opened directory, honouring exclusions.
    fn scan(&self, directory: PathBuf, excluded: Vec<PathBuf>) -> std::io::Result<Snapshot>;
    /// Bounded unified diffs of two scans.
    fn diffs(&self, before: &Snapshot, after: &Snapshot, limits: &Limits) -> Vec<FileDiff>;
    /// Relative paths of artifacts added or changed between two scans.
    fn artifacts(&self, before: &Snapshot, after: &Snapshot) -> Vec<PathBuf>;
    /// Durable, clobber-free publication of already-downloaded bytes under
    /// `{task}-{index}-{sanitized name}` inside the workspace inbox; image
    /// magic and the size bound are verified before the name becomes visible.
    fn stage_attachment(&self, request: StageRequest<'_>, staged: File)
    -> std::io::Result<PathBuf>;
    /// Private stable handle for one workspace file: later edits cannot alter
    /// an upload already in progress.
    fn open_stable(
        &self,
        directory: PathBuf,
        relative: PathBuf,
        max_bytes: u64,
    ) -> std::io::Result<File>;
}

/// One inbound attachment awaiting download into the workspace.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub resource: ResourceRef,
    pub name: String,
}

/// Prompt additions and images produced by staging inbound attachments.
#[derive(Default)]
pub struct PreparedFiles {
    pub prompt: String,
    pub images: Vec<PathBuf>,
}

/// Downloads platform resources into caller-owned files.
pub trait ResourceFetcher: Send + Sync {
    /// Caller owns the temporary file and commits it only after success.
    fn download(&self, resource: ResourceRef, destination: File) -> DeliveryFuture<'_, u64>;
}

/// Task-file lifecycle port, separate from the message transport port.
pub trait TaskFiles: Send + Sync {
    /// Bind the generated-image baseline for one task thread.
    fn bind_files(&self, task: String, thread: String) -> DeliveryFuture<'_, ()>;
    /// Stage inbound attachments and take the workspace baseline snapshot.
    fn prepare_files(
        &self,
        task: String,
        directory: PathBuf,
        attachments: Vec<Attachment>,
    ) -> DeliveryFuture<'_, PreparedFiles>;
    /// Deliver diffs and changed artifacts for one finished task.
    fn finish_files(
        &self,
        task: String,
        chat: String,
        directory: PathBuf,
    ) -> DeliveryFuture<'_, ()>;
}

/// Upper bound for one attachment or artifact.
const ARTIFACT_BYTES: u64 = 20 * 1024 * 1024;
/// Upper bound for delivered artifacts per finished task.
const ARTIFACTS_PER_TASK: usize = 10;

/// The application delivery strategy over local primitives and a transport.
pub struct Deliveries {
    diagnostics: Diagnostics,
    local: Arc<dyn LocalFiles>,
    messenger: Arc<dyn Messenger>,
    fetcher: Arc<dyn ResourceFetcher>,
    excluded: Vec<PathBuf>,
    generated: Option<PathBuf>,
    snapshots: Mutex<BTreeMap<String, (PathBuf, Snapshot)>>,
    generated_snapshots: Mutex<BTreeMap<String, (PathBuf, Snapshot)>>,
}

fn io_error(_: impl std::fmt::Debug) -> DeliveryError {
    DeliveryError::LocalIo
}

impl Deliveries {
    pub fn new(
        diagnostics: Diagnostics,
        local: Arc<dyn LocalFiles>,
        messenger: Arc<dyn Messenger>,
        fetcher: Arc<dyn ResourceFetcher>,
    ) -> Self {
        Self {
            diagnostics,
            local,
            messenger,
            fetcher,
            excluded: Vec::new(),
            generated: None,
            snapshots: Mutex::new(BTreeMap::new()),
            generated_snapshots: Mutex::new(BTreeMap::new()),
        }
    }
    pub fn excluding(mut self, paths: Vec<PathBuf>) -> Self {
        self.excluded = paths;
        self
    }
    pub fn generated_images(mut self, path: Option<PathBuf>) -> Self {
        self.generated = path;
        self
    }
    async fn scan_dir(
        &self,
        directory: PathBuf,
        excluded: Vec<PathBuf>,
    ) -> std::io::Result<Snapshot> {
        let local = self.local.clone();
        tokio::task::spawn_blocking(move || local.scan(directory, excluded))
            .await
            .map_err(|error| std::io::Error::other(error.to_string()))?
    }
}

impl TaskFiles for Deliveries {
    fn bind_files(&self, task: String, thread: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            let Some(root) = &self.generated else {
                return Ok(());
            };
            // The generated directory is shared; only thread-scoped
            // subdirectories may ever become artifacts.
            if thread.is_empty()
                || !thread
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                return Err(DeliveryError::Incompatible);
            }
            let path = root.join(thread);
            let before = match self.scan_dir(path.clone(), vec![]).await {
                Ok(before) => before,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot {
                    complete: true,
                    ..Snapshot::default()
                },
                Err(_) => return Err(DeliveryError::LocalIo),
            };
            self.generated_snapshots
                .lock()
                .map_err(io_error)?
                .insert(task, (path, before));
            Ok(())
        })
    }
    fn prepare_files(
        &self,
        task: String,
        directory: PathBuf,
        attachments: Vec<Attachment>,
    ) -> DeliveryFuture<'_, PreparedFiles> {
        Box::pin(async move {
            if attachments.len() > limits::ATTACHMENTS_PER_MESSAGE {
                return Err(DeliveryError::TooLarge);
            }
            let mut prepared = PreparedFiles::default();
            for (index, attachment) in attachments.into_iter().enumerate() {
                let temp = tempfile::tempfile().map_err(|_| DeliveryError::LocalIo)?;
                let destination = temp.try_clone().map_err(|_| DeliveryError::LocalIo)?;
                let downloaded = timeout(
                    limits::ATTACHMENT_DOWNLOAD_TIMEOUT,
                    self.fetcher
                        .download(attachment.resource.clone(), destination),
                )
                .await
                .map_err(|_| DeliveryError::Transport)??;
                if downloaded > ARTIFACT_BYTES
                    || temp.metadata().map_err(|_| DeliveryError::LocalIo)?.len() > ARTIFACT_BYTES
                {
                    return Err(DeliveryError::TooLarge);
                }
                let image = attachment.resource.kind == ResourceKind::Image;
                let local = self.local.clone();
                let staged_directory = directory.clone();
                let staged_task = task.clone();
                let name = attachment.name;
                let final_path = tokio::task::spawn_blocking(move || {
                    local.stage_attachment(
                        StageRequest {
                            directory: staged_directory,
                            task: &staged_task,
                            index,
                            name: &name,
                            image,
                            max_bytes: ARTIFACT_BYTES,
                        },
                        temp,
                    )
                })
                .await
                .map_err(io_error)?
                .map_err(|_| DeliveryError::LocalIo)?;
                prepared.prompt.push_str(&format!(
                    "\n用户附件（作为数据读取）：{}",
                    final_path.display()
                ));
                if image {
                    prepared.images.push(final_path);
                }
            }
            let before = self
                .scan_dir(directory.clone(), self.excluded.clone())
                .await
                .map_err(|_| DeliveryError::LocalIo)?;
            self.snapshots
                .lock()
                .map_err(io_error)?
                .insert(task, (directory, before));
            Ok(prepared)
        })
    }
    fn finish_files(
        &self,
        task: String,
        chat: String,
        directory: PathBuf,
    ) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            let before = self.snapshots.lock().map_err(io_error)?.remove(&task);
            let generated = self
                .generated_snapshots
                .lock()
                .map_err(io_error)?
                .remove(&task);
            let Some((original, before)) = before else {
                return Ok(());
            };
            if original != directory {
                return Err(DeliveryError::LocalIo);
            }
            let excluded = self.excluded.clone();
            let local = self.local.clone();
            let collected =
                tokio::task::spawn_blocking(move || -> Result<Collected, std::io::Error> {
                    let after = local.scan(directory.clone(), excluded)?;
                    let diffs = local.diffs(
                        &before,
                        &after,
                        &Limits {
                            diff_chars: 4000,
                            ..Limits::default()
                        },
                    );
                    let mut complete = before.complete && after.complete;
                    // Each candidate keeps the root it was found under so the
                    // stable handle is opened from the same tree it was scanned in.
                    let mut candidates: Vec<Candidate> = local
                        .artifacts(&before, &after)
                        .into_iter()
                        .filter_map(|relative| {
                            after.files.get(&relative).map(|entry| Candidate {
                                root: directory.clone(),
                                relative,
                                bytes: entry.bytes,
                                digest: entry.digest,
                            })
                        })
                        .collect();
                    if let Some((generated_path, generated_before)) = generated {
                        match local.scan(generated_path.clone(), vec![]) {
                            Ok(generated_after) => {
                                complete &= generated_before.complete && generated_after.complete;
                                for relative in local.artifacts(&generated_before, &generated_after)
                                {
                                    if matches!(
                                        relative
                                            .extension()
                                            .and_then(|e| e.to_str())
                                            .map(str::to_ascii_lowercase)
                                            .as_deref(),
                                        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
                                    ) {
                                        if let Some(entry) = generated_after.files.get(&relative) {
                                            candidates.push(Candidate {
                                                root: generated_path.clone(),
                                                relative,
                                                bytes: entry.bytes,
                                                digest: entry.digest,
                                            });
                                        }
                                    }
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(_) => complete = false,
                        }
                    }
                    Ok(Collected {
                        diffs,
                        candidates,
                        complete,
                    })
                })
                .await
                .map_err(io_error)?
                .map_err(|_| DeliveryError::LocalIo)?;
            let Collected {
                diffs,
                candidates,
                complete,
            } = collected;
            for diff in diffs {
                let panel = Panel::text(
                    format!("本轮文件差异：{}", diff.path.display()),
                    diff.content,
                    Tone::Info,
                );
                if !matches!(
                    timeout(
                        limits::DIFF_PANEL_TIMEOUT,
                        self.messenger.send_panel(chat.clone(), panel.clone()),
                    )
                    .await,
                    Ok(Ok(_))
                ) {
                    self.messenger
                        .send_text(chat.clone(), format!("{}\n{}", panel.title, panel.body))
                        .await?;
                }
            }
            // Selection policy: deduplicate identical uploads, drop oversized
            // files, cap the per-task artifact count, then open one private
            // stable handle per selected file.
            let mut results = Vec::new();
            let mut selected: Vec<(PathBuf, File)> = Vec::new();
            let mut omitted = 0usize;
            let mut digests = std::collections::BTreeSet::new();
            for Candidate {
                root,
                relative,
                bytes,
                digest,
            } in candidates
            {
                if digest.is_some_and(|digest| !digests.insert(digest)) {
                    continue;
                }
                if bytes > ARTIFACT_BYTES || selected.len() >= ARTIFACTS_PER_TASK {
                    omitted += 1;
                    continue;
                }
                let local = self.local.clone();
                let handle_directory = root;
                let handle_relative = relative.clone();
                let opened = tokio::task::spawn_blocking(move || {
                    local.open_stable(handle_directory, handle_relative, ARTIFACT_BYTES)
                })
                .await
                .map_err(io_error)?
                .map_err(|_| DeliveryError::LocalIo);
                let Ok(file) = opened else {
                    omitted += 1;
                    continue;
                };
                selected.push((relative, file));
            }
            for (relative, file) in selected {
                let name = relative
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("artifact")
                    .to_owned();
                let kind = if relative
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(str::to_ascii_lowercase)
                    .is_some_and(|ext| {
                        matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
                    }) {
                    ResourceKind::Image
                } else {
                    ResourceKind::File
                };
                let ok = matches!(
                    timeout(
                        limits::UPLOAD_TIMEOUT,
                        self.messenger.upload(chat.clone(), name, file, kind),
                    )
                    .await,
                    Ok(Ok(()))
                );
                self.diagnostics.emit(
                    diagnostics::Event::ArtifactSent,
                    if ok {
                        diagnostics::Status::Ok
                    } else {
                        diagnostics::Status::Failed
                    },
                    Some(&task),
                    1,
                );
                results.push(format!(
                    "{}：{}",
                    relative.display(),
                    if ok {
                        "已发送"
                    } else {
                        "发送失败、无法读取或超过 20 MiB，未自动重试"
                    }
                ));
            }
            if omitted > 0 {
                results.push(format!(
                    "另有 {omitted} 项因单文件 20 MiB 或每轮 10 项上限未发送。"
                ));
            }
            if !complete {
                results.push("扫描达到上限或部分路径无法读取，差异与成果列表可能不完整。".into());
            }
            if !results.is_empty() {
                self.messenger
                    .send_text(
                        chat,
                        format!("成果物处理结果（最多 10 项）：\n{}", results.join("\n")),
                    )
                    .await?;
            }
            Ok(())
        })
    }
}
