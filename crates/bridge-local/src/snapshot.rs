//! Bounded snapshots and delivery classification. Never upload engineering text.
//!
//! The snapshot data model lives in [`bridge_app::files`]; this module owns the
//! scanning, diffing and safe-open algorithms over it.
use bridge_app::files::{Entry, FileDiff, FileKind, Limits, Snapshot};

use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::{
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

/// Open every relative path component without following symlinks; see
/// [`crate::safeio`] for the shared threat model.
pub fn open_regular(root: &File, relative: &Path) -> io::Result<File> {
    crate::safeio::open_regular(root, relative)
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
        if kind == FileKind::Artifact
            && metadata.len() <= ARTIFACT_HASH_LIMIT
            && hash_artifact(&mut file, &mut state).is_err()
        {
            result.complete = false;
        }
        if kind == FileKind::Text {
            read_text_entry(&mut file, limits.text_bytes, &mut state);
        }
        result.files.insert(relative.into(), state);
    }
    Ok(result)
}

/// Largest artifact that is hashed instead of only measured.
const ARTIFACT_HASH_LIMIT: u64 = 20 * 1024 * 1024;

/// SHA-256 an artifact file, marking the entry incomplete when it exceeds
/// [`ARTIFACT_HASH_LIMIT`] mid-read or the read fails.
fn hash_artifact(file: &mut File, state: &mut Entry) -> io::Result<()> {
    let mut hash = Sha256::new();
    let mut reader = Read::by_ref(file).take(ARTIFACT_HASH_LIMIT + 1);
    let mut buffer = [0u8; 64 * 1024];
    let mut count = 0u64;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                state.digest = Some(hash.finalize().into());
                return Ok(());
            }
            Ok(n) => {
                count += n as u64;
                hash.update(&buffer[..n]);
            }
            Err(error) => return Err(error),
        }
        if count > ARTIFACT_HASH_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "artifact too large",
            ));
        }
    }
}

/// Read a bounded text entry and classify why it is not renderable; the skip
/// reasons are user-facing labels, not error reports.
fn read_text_entry(file: &mut File, text_bytes: usize, state: &mut Entry) {
    let mut bytes = Vec::new();
    match Read::by_ref(file)
        .take(text_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
    {
        Err(_) => state.skipped = Some("无法读取".into()),
        Ok(_) if bytes.len() > text_bytes => state.skipped = Some("文件过大".into()),
        Ok(_) if bytes.contains(&0) => state.skipped = Some("二进制文件".into()),
        Ok(_) => match String::from_utf8(bytes) {
            Ok(text) => state.text = Some(text),
            Err(_) => state.skipped = Some("非 UTF-8 文本".into()),
        },
    }
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
