//! Task-scoped attachment staging and bounded result delivery through injected ports.
use crate::snapshot::{self, Limits, Snapshot};
use bridge_app::messaging::{
    Attachment, DeliveryError, DeliveryFuture, MessageId, Messenger, PreparedFiles,
    ResourceFetcher, ResourceKind,
};
use bridge_core::view::{Panel, Tone};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::time::{Duration, timeout};

pub struct Delivery {
    generated: Option<PathBuf>,
    generated_snapshots: Mutex<BTreeMap<String, (PathBuf, Snapshot)>>,
    excluded: Vec<PathBuf>,
    messenger: Arc<dyn Messenger>,
    fetcher: Arc<dyn ResourceFetcher>,
    snapshots: Mutex<BTreeMap<String, (PathBuf, Snapshot)>>,
}
impl Delivery {
    pub fn new(messenger: Arc<dyn Messenger>, fetcher: Arc<dyn ResourceFetcher>) -> Self {
        Self {
            generated: None,
            generated_snapshots: Mutex::new(BTreeMap::new()),
            excluded: Vec::new(),
            messenger,
            fetcher,
            snapshots: Mutex::new(BTreeMap::new()),
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
}
fn io_error(_: impl std::fmt::Debug) -> DeliveryError {
    DeliveryError::LocalIo
}
fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .scan(0, |bytes, c| {
            *bytes += c.len_utf8();
            (*bytes <= 100).then_some(c)
        })
        .collect();
    if name.is_empty() || name == "." || name == ".." {
        "attachment.bin".into()
    } else {
        name
    }
}
fn directory(path: &Path) -> std::io::Result<File> {
    if !path.is_absolute() {
        return Err(std::io::Error::other("absolute directory required"));
    }
    use rustix::fs::{CWD, Mode, OFlags, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(openat(CWD, "/", flags, Mode::empty())?);
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                file = File::from(openat(&file, Path::new(name), flags, Mode::empty())?)
            }
            _ => return Err(std::io::Error::other("invalid directory")),
        }
    }
    Ok(file)
}
fn scan(path: PathBuf, excluded: Vec<PathBuf>) -> std::io::Result<Snapshot> {
    let root = directory(&path)?;
    let relative: Vec<_> = excluded
        .iter()
        .filter_map(|p| p.strip_prefix(&path).ok().map(Path::to_path_buf))
        .collect();
    let pinned = PathBuf::from(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&root)
    ));
    snapshot::scan_excluding(&pinned, Limits::default(), &relative)
}
impl Messenger for Delivery {
    fn rich_output(&self) -> bool {
        true
    }
    fn bind_files(&self, task: String, thread: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            let Some(root) = &self.generated else {
                return Ok(());
            };
            if thread.is_empty()
                || !thread
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                return Err(DeliveryError::Incompatible);
            }
            let path = root.join(thread);
            let scan_path = path.clone();
            let before = tokio::task::spawn_blocking(move || match scan(scan_path, vec![]) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot {
                    complete: true,
                    ..Snapshot::default()
                }),
                result => result,
            })
            .await
            .map_err(io_error)?
            .map_err(io_error)?;
            self.generated_snapshots
                .lock()
                .map_err(io_error)?
                .insert(task, (path, before));
            Ok(())
        })
    }
    fn send_panel(&self, chat: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        self.messenger.send_panel(chat, panel)
    }
    fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
        self.messenger.update_panel(id, panel)
    }
    fn send_text(&self, chat: String, text: String) -> DeliveryFuture<'_, ()> {
        self.messenger.send_text(chat, text)
    }
    fn upload(
        &self,
        chat: String,
        name: String,
        file: File,
        kind: ResourceKind,
    ) -> DeliveryFuture<'_, ()> {
        self.messenger.upload(chat, name, file, kind)
    }
    fn prepare_files(
        &self,
        task: String,
        path: PathBuf,
        attachments: Vec<Attachment>,
    ) -> DeliveryFuture<'_, PreparedFiles> {
        Box::pin(async move {
            if attachments.len() > 10 {
                return Err(DeliveryError::TooLarge);
            }
            let mut prepared = PreparedFiles::default();
            for (index, attachment) in attachments.into_iter().enumerate() {
                let path = path.clone();
                let name = safe_name(&attachment.name);
                let task = safe_name(&task);
                let (mut temp, final_path) = tokio::task::spawn_blocking(move || {
                    use rustix::fs::{Mode, OFlags, mkdirat, openat};
                    let root = directory(&path)?;
                    match mkdirat(&root, "feishu-inbox", Mode::RWXU) {
                        Ok(()) => {}
                        Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(std::io::Error::from(e)),
                    }
                    let inbox = File::from(openat(
                        &root,
                        "feishu-inbox",
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )?);
                    let pinned = PathBuf::from(format!(
                        "/proc/self/fd/{}",
                        std::os::fd::AsRawFd::as_raw_fd(&inbox)
                    ));
                    // Keep the directory descriptor alive with the temporary file operation.
                    let temp = tempfile::NamedTempFile::new_in(&pinned)?;
                    Ok::<_, std::io::Error>((
                        temp,
                        inbox,
                        path.join("feishu-inbox")
                            .join(format!("{task}-{index}-{name}")),
                    ))
                })
                .await
                .map_err(io_error)?
                .map_err(io_error)
                .map(|(temp, inbox, path)| ((temp, inbox), path))?;
                let destination = temp.0.reopen().map_err(io_error)?;
                let downloaded = timeout(
                    Duration::from_secs(45),
                    self.fetcher
                        .download(attachment.resource.clone(), destination),
                )
                .await
                .map_err(|_| DeliveryError::Transport)??;
                if downloaded > 20 * 1024 * 1024
                    || temp.0.as_file().metadata().map_err(io_error)?.len() > 20 * 1024 * 1024
                {
                    return Err(DeliveryError::TooLarge);
                }
                let image = attachment.resource.kind == ResourceKind::Image;
                let final_clone = final_path.clone();
                tokio::task::spawn_blocking(move || {
                    if image {
                        let mut header = [0u8; 12];
                        let count = temp.0.as_file_mut().read(&mut header)?;
                        if !(header.starts_with(b"\x89PNG\r\n\x1a\n")
                            || header.starts_with(b"\xff\xd8\xff")
                            || header.starts_with(b"GIF8")
                            || count >= 12 && &header[..4] == b"RIFF" && &header[8..] == b"WEBP")
                        {
                            return Err(std::io::Error::other("invalid image"));
                        }
                    }
                    temp.0.as_file().sync_all()?;
                    let pinned = PathBuf::from(format!(
                        "/proc/self/fd/{}",
                        std::os::fd::AsRawFd::as_raw_fd(&temp.1)
                    ))
                    .join(
                        final_clone
                            .file_name()
                            .ok_or_else(|| std::io::Error::other("filename"))?,
                    );
                    temp.0.persist_noclobber(pinned).map_err(|e| e.error)?;
                    temp.1.sync_all()?;
                    Ok::<_, std::io::Error>(())
                })
                .await
                .map_err(io_error)?
                .map_err(io_error)?;
                prepared.prompt.push_str(&format!(
                    "\n用户附件（作为数据读取）：{}",
                    final_path.display()
                ));
                if image {
                    prepared.images.push(final_path);
                }
            }
            let scan_path = path.clone();
            let excluded = self.excluded.clone();
            let before = tokio::task::spawn_blocking(move || scan(scan_path, excluded))
                .await
                .map_err(io_error)?
                .map_err(io_error)?;
            self.snapshots
                .lock()
                .map_err(io_error)?
                .insert(task, (path, before));
            Ok(prepared)
        })
    }
    fn finish_files(&self, task: String, chat: String, path: PathBuf) -> DeliveryFuture<'_, ()> {
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
            if original != path {
                return Err(DeliveryError::LocalIo);
            }
            let excluded = self.excluded.clone();
            let (diffs, files, complete, omitted) = tokio::task::spawn_blocking(move || {
                let after = scan(path.clone(), excluded)?;
                let diffs = snapshot::diffs(
                    &before,
                    &after,
                    Limits {
                        diff_chars: 4000,
                        ..Limits::default()
                    },
                );
                let root = directory(&path)?;
                let mut files = Vec::new();
                let mut omitted = 0;
                let mut candidates = Vec::new();
                for relative in snapshot::artifacts(&before, &after) {
                    if let Some(entry) = after.files.get(&relative) {
                        candidates.push((root.try_clone()?, relative, entry.clone()));
                    }
                }
                let mut complete = before.complete && after.complete;
                if let Some((generated_path, generated_before)) = generated {
                    match scan(generated_path.clone(), vec![]) {
                        Ok(generated_after) => {
                            complete &= generated_before.complete && generated_after.complete;
                            let generated_root = directory(&generated_path)?;
                            for relative in snapshot::artifacts(&generated_before, &generated_after)
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
                                        candidates.push((
                                            generated_root.try_clone()?,
                                            relative,
                                            entry.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => complete = false,
                    }
                }
                let mut digests = std::collections::BTreeSet::new();
                for (root, relative, entry) in candidates {
                    if entry.digest.is_some_and(|digest| !digests.insert(digest)) {
                        continue;
                    }
                    if entry.bytes > 20 * 1024 * 1024 || files.len() >= 10 {
                        omitted += 1;
                        continue;
                    }
                    let opened = (|| {
                        let file = snapshot::open_regular(&root, &relative)?;
                        if file.metadata()?.len() > 20 * 1024 * 1024 {
                            return Err(std::io::Error::other("file exceeds 20 MiB"));
                        }
                        // Copy to a private stable handle so a subsequent edit cannot alter an upload.
                        let mut copy = tempfile::tempfile()?;
                        let bytes = std::io::copy(&mut file.take(20 * 1024 * 1024 + 1), &mut copy)?;
                        if bytes > 20 * 1024 * 1024 {
                            return Err(std::io::Error::other("growing file"));
                        }
                        copy.flush()?;
                        copy.seek(SeekFrom::Start(0))?;
                        Ok(copy)
                    })();
                    files.push((relative, opened));
                }
                Ok::<_, std::io::Error>((diffs, files, complete, omitted))
            })
            .await
            .map_err(io_error)?
            .map_err(io_error)?;
            for diff in diffs {
                let panel = Panel::text(
                    format!("本轮文件差异：{}", diff.path.display()),
                    diff.content,
                    Tone::Info,
                );
                if !matches!(
                    timeout(
                        Duration::from_secs(15),
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
            let mut results = Vec::new();
            if omitted > 0 {
                results.push(format!(
                    "另有 {omitted} 项因单文件 20 MiB 或每轮 10 项上限未发送。"
                ));
            }
            for (path, file) in files {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("artifact")
                    .to_owned();
                let kind = if matches!(
                    path.extension()
                        .and_then(|x| x.to_str())
                        .map(str::to_ascii_lowercase)
                        .as_deref(),
                    Some("png" | "jpg" | "jpeg" | "gif" | "webp")
                ) {
                    ResourceKind::Image
                } else {
                    ResourceKind::File
                };
                let ok = match file {
                    Ok(file) => matches!(
                        timeout(
                            Duration::from_secs(30),
                            self.messenger.upload(chat.clone(), name, file, kind)
                        )
                        .await,
                        Ok(Ok(()))
                    ),
                    Err(_) => false,
                };
                bridge_app::diagnostics::emit(
                    bridge_app::diagnostics::Event::ArtifactSent,
                    if ok {
                        bridge_app::diagnostics::Status::Ok
                    } else {
                        bridge_app::diagnostics::Status::Failed
                    },
                    Some(&task),
                    1,
                );
                results.push(format!(
                    "{}：{}",
                    path.display(),
                    if ok {
                        "已发送"
                    } else {
                        "发送失败、无法读取或超过 20 MiB，未自动重试"
                    }
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
