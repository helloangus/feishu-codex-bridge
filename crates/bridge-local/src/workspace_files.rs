//! Safe workspace file primitives: bounded scans, durable attachment staging
//! and private stable handles. No messaging types and no strategy decisions.
use bridge_app::files::{Limits, LocalFiles, Snapshot};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// Stateless filesystem primitives shared by every workspace.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkspaceFiles;

/// Upper bound for a sanitized name component.
const NAME_BYTES: usize = 100;

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
            (*bytes <= NAME_BYTES).then_some(c)
        })
        .collect();
    if name.is_empty() || name == "." || name == ".." {
        "attachment.bin".into()
    } else {
        name
    }
}

/// Open every path component without following symlinks; rejects relative or
/// special paths so later operations stay inside the intended directory.
fn safe_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute directory required"));
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
            _ => return Err(io::Error::other("invalid directory")),
        }
    }
    Ok(file)
}

/// Scan a safely opened directory through its pinned descriptor.
fn scan_directory(path: PathBuf, excluded: Vec<PathBuf>) -> io::Result<Snapshot> {
    let root = safe_directory(&path)?;
    let relative: Vec<_> = excluded
        .iter()
        .filter_map(|p| p.strip_prefix(&path).ok().map(Path::to_path_buf))
        .collect();
    let pinned = PathBuf::from(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&root)
    ));
    super::snapshot::scan_excluding(&pinned, Limits::default(), &relative)
}

fn image_magic(file: &mut File) -> io::Result<bool> {
    let mut header = [0u8; 12];
    let count = file.read(&mut header)?;
    Ok(header.starts_with(b"\x89PNG\r\n\x1a\n")
        || header.starts_with(b"\xff\xd8\xff")
        || header.starts_with(b"GIF8")
        || count >= 12 && &header[..4] == b"RIFF" && &header[8..] == b"WEBP")
}

impl LocalFiles for WorkspaceFiles {
    fn scan(&self, directory: PathBuf, excluded: Vec<PathBuf>) -> io::Result<Snapshot> {
        scan_directory(directory, excluded)
    }
    fn diffs(
        &self,
        before: &Snapshot,
        after: &Snapshot,
        limits: &Limits,
    ) -> Vec<bridge_app::files::FileDiff> {
        super::snapshot::diffs(before, after, *limits)
    }
    fn artifacts(&self, before: &Snapshot, after: &Snapshot) -> Vec<PathBuf> {
        super::snapshot::artifacts(before, after)
    }
    fn stage_attachment(
        &self,
        request: bridge_app::files::StageRequest<'_>,
        mut staged: File,
    ) -> io::Result<PathBuf> {
        use rustix::fs::{Mode, OFlags, mkdirat, openat};
        let bridge_app::files::StageRequest {
            directory,
            task,
            index,
            name,
            image,
            max_bytes,
        } = request;
        let final_name = format!("{}-{index}-{}", safe_name(task), safe_name(name));
        let root = safe_directory(&directory)?;
        match mkdirat(&root, "feishu-inbox", Mode::RWXU) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(io::Error::from(e)),
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
        let mut temp = tempfile::NamedTempFile::new_in(&pinned)?;
        staged.seek(SeekFrom::Start(0))?;
        let copied = io::copy(
            &mut staged.take(max_bytes.saturating_add(1)),
            temp.as_file_mut(),
        )?;
        if copied > max_bytes || temp.as_file().metadata()?.len() > max_bytes {
            return Err(io::Error::other("attachment exceeds limit"));
        }
        if image {
            temp.as_file_mut().seek(SeekFrom::Start(0))?;
            if !image_magic(temp.as_file_mut())? {
                return Err(io::Error::other("invalid image"));
            }
        }
        temp.as_file().sync_all()?;
        temp.persist_noclobber(pinned.join(&final_name))
            .map_err(|e| e.error)?;
        inbox.sync_all()?;
        Ok(directory.join("feishu-inbox").join(final_name))
    }
    fn open_stable(
        &self,
        directory: PathBuf,
        relative: PathBuf,
        max_bytes: u64,
    ) -> io::Result<File> {
        let root = safe_directory(&directory)?;
        let file = super::snapshot::open_regular(&root, &relative)?;
        if file.metadata()?.len() > max_bytes {
            return Err(io::Error::other("file exceeds limit"));
        }
        // Copy to a private stable handle so a subsequent edit cannot alter an
        // upload that is already in progress.
        let mut copy = tempfile::tempfile()?;
        let bytes = io::copy(&mut file.take(max_bytes.saturating_add(1)), &mut copy)?;
        if bytes > max_bytes {
            return Err(io::Error::other("growing file"));
        }
        copy.flush()?;
        copy.seek(SeekFrom::Start(0))?;
        Ok(copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn sanitized_names_refuse_traversal_and_empty_results() {
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name(""), "attachment.bin");
        assert_eq!(safe_name("任务/报告.pdf"), "任务_报告.pdf");
        assert!(safe_name("长").len() <= NAME_BYTES);
    }

    #[test]
    fn scans_ignore_inbox_and_exclusions() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("main.rs"), "fn main() {}\n")?;
        fs::create_dir(temp.path().join("feishu-inbox"))?;
        fs::write(temp.path().join("feishu-inbox/pending.png"), b"image")?;
        fs::create_dir(temp.path().join("private"))?;
        fs::write(temp.path().join("private/secret.txt"), "s")?;
        let snapshot =
            WorkspaceFiles.scan(temp.path().to_path_buf(), vec![temp.path().join("private")])?;
        assert!(snapshot.files.contains_key(Path::new("main.rs")));
        assert!(
            !snapshot
                .files
                .contains_key(Path::new("feishu-inbox/pending.png"))
        );
        assert!(!snapshot.files.contains_key(Path::new("private/secret.txt")));
        assert!(snapshot.complete);
        Ok(())
    }

    fn staged(bytes: &[u8]) -> io::Result<File> {
        let file = tempfile::tempfile()?;
        (&file).write_all(bytes)?;
        Ok(file)
    }

    #[test]
    fn staging_publishes_durably_and_rejects_invalid_images_or_oversize() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let workspace = WorkspaceFiles;
        let png: &[u8] = b"\x89PNG\r\n\x1a\nbody";
        let final_path = workspace.stage_attachment(
            bridge_app::files::StageRequest {
                directory: temp.path().to_path_buf(),
                task: "81:1",
                index: 0,
                name: "../../测试.png",
                image: true,
                max_bytes: 1024,
            },
            staged(png)?,
        )?;
        assert_eq!(
            final_path,
            temp.path()
                .join("feishu-inbox")
                .join("81_1-0-.._.._测试.png")
        );
        assert_eq!(fs::read(&final_path)?, png);
        // A non-image refused for an image slot and an oversized payload never
        // leave a published name behind.
        let inbox = temp.path().join("feishu-inbox");
        assert!(
            workspace
                .stage_attachment(
                    bridge_app::files::StageRequest {
                        directory: temp.path().to_path_buf(),
                        task: "t",
                        index: 1,
                        name: "a.png",
                        image: true,
                        max_bytes: 1024,
                    },
                    staged(b"not an image")?,
                )
                .is_err()
        );
        assert!(
            workspace
                .stage_attachment(
                    bridge_app::files::StageRequest {
                        directory: temp.path().to_path_buf(),
                        task: "t",
                        index: 2,
                        name: "b.bin",
                        image: false,
                        max_bytes: 8,
                    },
                    staged(b"way too many bytes")?,
                )
                .is_err()
        );
        assert_eq!(fs::read_dir(&inbox)?.count(), 1);
        Ok(())
    }

    #[test]
    fn stable_handles_survive_later_edits_and_enforce_size() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let workspace = WorkspaceFiles;
        fs::write(temp.path().join("doc.pdf"), b"first")?;
        let handle =
            workspace.open_stable(temp.path().to_path_buf(), PathBuf::from("doc.pdf"), 1024)?;
        fs::write(temp.path().join("doc.pdf"), b"second edit")?;
        let mut bytes = Vec::new();
        (&handle).take(1024).read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"first");
        assert!(
            workspace
                .open_stable(temp.path().to_path_buf(), PathBuf::from("doc.pdf"), 2,)
                .is_err()
        );
        assert!(
            workspace
                .open_stable(temp.path().to_path_buf(), PathBuf::from("../escape"), 1024)
                .is_err()
        );
        Ok(())
    }
}
