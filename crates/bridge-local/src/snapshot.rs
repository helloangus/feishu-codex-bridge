//! Bounded snapshots and delivery classification. Never upload engineering text.
use rustix::fs::{Mode, OFlags, openat};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::{Component, Path},
};
use walkdir::WalkDir;

const IGNORE_DIRS: &[&str] = &[
    ".git",
    ".runtime",
    "feishu-inbox",
    "__pycache__",
    ".venv",
    "venv",
    "node_modules",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".cache",
    "target",
    "build",
    "dist",
    ".bridge-state",
];
const IGNORE_EXTS: &[&str] = &["pyc", "pyo", "o", "obj", "class", "so", "dll", "a", "lib"];
const ARTIFACT_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "pdf", "doc", "docx", "xls", "xlsx",
    "ppt", "pptx", "odt", "ods", "mp3", "wav", "ogg", "m4a", "mp4", "mov", "webm", "zip", "tar",
    "gz", "bz2", "xz", "7z",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Ignore,
    Text,
    Artifact,
}

pub fn file_kind(relative: &Path) -> FileKind {
    if relative.components().any(|part| match part {
        Component::Normal(name) => IGNORE_DIRS
            .iter()
            .any(|ignored| name == std::ffi::OsStr::new(ignored)),
        _ => true,
    }) {
        return FileKind::Ignore;
    }
    let name = relative.file_name().and_then(|v| v.to_str()).unwrap_or("");
    let ext = relative
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.starts_with(".feishu-codex")
        || name == ".env"
        || (name.starts_with(".env.") && name != ".env.example")
        || IGNORE_EXTS.contains(&ext.as_str())
    {
        return FileKind::Ignore;
    }
    if ARTIFACT_EXTS.contains(&ext.as_str()) {
        FileKind::Artifact
    } else {
        FileKind::Text
    }
}

/// Open every relative path component without following symlinks. Nonblocking
/// final open prevents a swapped FIFO from hanging the scanner.
pub fn open_regular(root: &File, relative: &Path) -> io::Result<File> {
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
    }
    let mut directory = root.try_clone()?;
    for (index, component) in parts.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "non-relative path",
            ));
        };
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if index + 1 < parts.len() {
            flags |= OFlags::DIRECTORY;
        }
        directory = File::from(openat(&directory, Path::new(name), flags, Mode::empty())?);
    }
    if !directory.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(directory)
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: FileKind,
    pub bytes: u64,
    pub modified: Option<std::time::SystemTime>,
    pub text: Option<String>,
    pub skipped: Option<String>,
    pub digest: Option<[u8; 32]>,
}
#[derive(Debug, Default)]
pub struct Snapshot {
    pub files: BTreeMap<std::path::PathBuf, Entry>,
    pub complete: bool,
}

pub fn scan(root: &Path, limits: Limits) -> io::Result<Snapshot> {
    scan_excluding(root, limits, &[])
}
pub fn scan_excluding(
    root: &Path,
    limits: Limits,
    excluded: &[std::path::PathBuf],
) -> io::Result<Snapshot> {
    let root_file = File::open(root)?;
    let mut result = Snapshot {
        complete: true,
        ..Snapshot::default()
    };
    let iterator = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || entry.path().strip_prefix(root).is_ok_and(|relative| {
                    file_kind(relative) != FileKind::Ignore
                        && !excluded.iter().any(|path| relative.starts_with(path))
                })
        });
    for (visited, entry) in iterator.enumerate() {
        if visited >= limits.entries {
            result.complete = false;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                result.complete = false;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if result.files.len() >= limits.files {
            result.complete = false;
            break;
        }
        let relative = entry.path().strip_prefix(root).map_err(io::Error::other)?;
        let kind = file_kind(relative);
        if kind == FileKind::Ignore {
            continue;
        }
        let mut file = match open_regular(&root_file, relative) {
            Ok(file) => file,
            Err(_) => {
                result.complete = false;
                continue;
            }
        };
        let metadata = file.metadata()?;
        let mut state = Entry {
            kind,
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            text: None,
            skipped: None,
            digest: None,
        };
        if kind == FileKind::Artifact && metadata.len() <= 20 * 1024 * 1024 {
            let mut hash = Sha256::new();
            let mut reader = Read::by_ref(&mut file).take(20 * 1024 * 1024 + 1);
            let mut buffer = [0u8; 64 * 1024];
            let mut count = 0u64;
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        state.digest = Some(hash.finalize().into());
                        break;
                    }
                    Ok(n) => {
                        count += n as u64;
                        hash.update(&buffer[..n]);
                    }
                    Err(_) => {
                        result.complete = false;
                        break;
                    }
                }
                if count > 20 * 1024 * 1024 {
                    result.complete = false;
                    break;
                }
            }
        }
        if kind == FileKind::Text {
            let mut bytes = Vec::new();
            match Read::by_ref(&mut file)
                .take(limits.text_bytes.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
            {
                Err(_) => state.skipped = Some("无法读取".into()),
                Ok(_) if bytes.len() > limits.text_bytes => state.skipped = Some("文件过大".into()),
                Ok(_) if bytes.contains(&0) => state.skipped = Some("二进制文件".into()),
                Ok(_) => match String::from_utf8(bytes) {
                    Ok(text) => state.text = Some(text),
                    Err(_) => state.skipped = Some("非 UTF-8 文本".into()),
                },
            }
        }
        result.files.insert(relative.into(), state);
    }
    Ok(result)
}

#[derive(Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: std::path::PathBuf,
    pub content: String,
}

fn changed(old: Option<&Entry>, new: &Entry) -> bool {
    match old {
        None => true,
        Some(old) if old.digest.is_some() && new.digest.is_some() => old.digest != new.digest,
        Some(old) => match (&old.text, &new.text) {
            (Some(a), Some(b)) => a != b,
            _ => {
                old.bytes != new.bytes || old.modified != new.modified || old.skipped != new.skipped
            }
        },
    }
}

pub fn artifacts(before: &Snapshot, after: &Snapshot) -> Vec<std::path::PathBuf> {
    after
        .files
        .iter()
        .filter(|(path, entry)| {
            entry.kind == FileKind::Artifact && changed(before.files.get(*path), entry)
        })
        .map(|(path, _)| path.clone())
        .collect()
}

pub fn diffs(before: &Snapshot, after: &Snapshot, limits: Limits) -> Vec<FileDiff> {
    let paths: std::collections::BTreeSet<_> =
        before.files.keys().chain(after.files.keys()).collect();
    let mut result = Vec::new();
    for path in paths {
        let old = before.files.get(path);
        let new = after.files.get(path);
        if new.is_none() && !after.complete {
            continue;
        }
        if new.is_some_and(|new| !changed(old, new)) {
            continue;
        }
        if old
            .or(new)
            .is_some_and(|entry| entry.kind != FileKind::Text)
        {
            continue;
        }
        let operation = if old.is_none() {
            if before.complete {
                "新增"
            } else {
                "变化（开始快照未完整覆盖）"
            }
        } else if new.is_none() {
            "删除"
        } else {
            "修改"
        };
        if let Some(reason) = old
            .into_iter()
            .chain(new)
            .find_map(|entry| entry.skipped.as_deref())
        {
            result.push(FileDiff {
                path: path.clone(),
                content: format!("{operation} · 无法生成文本差异：{reason}。未上传原文件。"),
            });
            continue;
        }
        let old_text = old.and_then(|entry| entry.text.as_deref()).unwrap_or("");
        let new_text = new.and_then(|entry| entry.text.as_deref()).unwrap_or("");
        let diff = TextDiff::from_lines(old_text, new_text);
        let mut added = 0;
        let mut removed = 0;
        for change in diff.iter_all_changes() {
            match change.tag() {
                ChangeTag::Insert => added += 1,
                ChangeTag::Delete => removed += 1,
                _ => {}
            }
        }
        let rendered = diff
            .unified_diff()
            .context_radius(3)
            .header(
                &format!("a/{}", path.display()),
                &format!("b/{}", path.display()),
            )
            .to_string();
        let mut bounded: String = rendered.chars().take(limits.diff_chars).collect();
        if bounded.len() < rendered.len() {
            bounded.push_str("\n…（差异已截断）");
        }
        let newline_note = if old_text.ends_with('\n') != new_text.ends_with('\n') {
            "\n文件末尾换行状态发生变化。"
        } else {
            ""
        };
        let fence = "`".repeat(
            bounded
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
                .max(3),
        );
        result.push(FileDiff {
            path: path.clone(),
            content: format!(
                "{operation} · +{added} / -{removed}{newline_note}\n\n{fence}diff\n{bounded}\n{fence}"
            ),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn artifact_content_detects_same_metadata_edits_and_ignores_timestamp_only_changes()
    -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("report.pdf");
        fs::write(&path, b"old")?;
        let before = scan(temp.path(), Limits::default())?;
        let modified = fs::metadata(&path)?.modified()?;
        fs::write(&path, b"new")?;
        File::options()
            .write(true)
            .open(&path)?
            .set_times(fs::FileTimes::new().set_modified(modified))?;
        let after = scan(temp.path(), Limits::default())?;
        assert_eq!(artifacts(&before, &after), [Path::new("report.pdf")]);
        File::options().write(true).open(&path)?.set_times(
            fs::FileTimes::new().set_modified(modified + std::time::Duration::from_secs(10)),
        )?;
        assert!(artifacts(&after, &scan(temp.path(), Limits::default())?).is_empty());
        Ok(())
    }
    #[test]
    fn deleted_text_and_newline_only_changes_are_reported() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("deleted.rs"), "old\n")?;
        fs::write(temp.path().join("newline.rs"), "same")?;
        let before = scan(temp.path(), Limits::default())?;
        fs::remove_file(temp.path().join("deleted.rs"))?;
        fs::write(temp.path().join("newline.rs"), "same\n")?;
        let changes = diffs(
            &before,
            &scan(temp.path(), Limits::default())?,
            Limits::default(),
        );
        assert_eq!(changes.len(), 2);
        assert!(changes[0].content.contains("删除") && changes[0].content.contains("-old"));
        assert!(changes[1].content.contains("换行状态"));
        Ok(())
    }
    #[test]
    fn engineering_text_diff_and_artifacts_remain_separate() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        fs::write(root.join("main.py"), "before\n")?;
        let before = scan(root, Limits::default())?;
        fs::write(root.join("main.py"), "after\n")?;
        fs::write(root.join("report.pdf"), b"\0pdf")?;
        fs::write(root.join(".env"), "secret")?;
        fs::create_dir(root.join("target"))?;
        fs::write(root.join("target/out"), "ignored")?;
        let after = scan(root, Limits::default())?;
        assert_eq!(after.files.len(), 2);
        assert_eq!(artifacts(&before, &after), [Path::new("report.pdf")]);
        let changes = diffs(&before, &after, Limits::default());
        assert_eq!(changes.len(), 1);
        assert!(changes[0].content.contains("-before"));
        assert!(changes[0].content.contains("+after"));
        Ok(())
    }
    #[test]
    fn truncated_scans_do_not_claim_deletions_or_upload_binary_text() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        fs::write(root.join("z.py"), "old")?;
        let before = scan(root, Limits::default())?;
        fs::write(root.join("a.bin"), b"\0binary")?;
        let limits = Limits {
            files: 1,
            ..Limits::default()
        };
        let after = scan(root, limits)?;
        assert!(!after.complete);
        let changes = diffs(&before, &after, limits);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].content.contains("二进制"));
        assert!(artifacts(&before, &after).is_empty());
        Ok(())
    }
    #[test]
    fn secure_open_rejects_symlinks_in_any_component() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        fs::create_dir(root.join("dir"))?;
        fs::write(root.join("dir/file"), "ok")?;
        let directory = File::open(root)?;
        assert!(open_regular(&directory, Path::new("dir/file")).is_ok());
        assert!(open_regular(&directory, Path::new("../file")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("dir"), root.join("link"))?;
            std::os::unix::fs::symlink(root.join("dir/file"), root.join("file"))?;
            assert!(open_regular(&directory, Path::new("link/file")).is_err());
            assert!(open_regular(&directory, Path::new("file")).is_err());
        }
        Ok(())
    }
}
