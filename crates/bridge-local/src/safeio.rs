//! The file-access safety boundary, named so the review surface is one module.
//!
//! Threat model: an attacker who can plant a symlink or swap a path component
//! between an open and a read (TOCTOU). Defense: every path component is
//! opened with `O_NOFOLLOW` relative to a directory descriptor already held
//! (`openat`), so a substituted link fails the open instead of silently
//! redirecting. All three primitives here must stay equivalent — audits only
//! need to check this file.
//!
//! Platform: Linux only (openat flags and `/proc/self/fd` pinning).
use std::{fs::File, io, os::fd::AsRawFd, path::Path};

/// Flags shared by every walk: no symlink substitution, no fd leaks, no
/// blocking on FIFOs.
const WALK_FLAGS_BASE: rustix::fs::OFlags = rustix::fs::OFlags::RDONLY
    .union(rustix::fs::OFlags::NOFOLLOW)
    .union(rustix::fs::OFlags::CLOEXEC)
    .union(rustix::fs::OFlags::NONBLOCK);

/// Walk every component of `relative` under an already-open `root` without
/// following symlinks. Intermediate components must be directories; the final
/// open does not require a type so callers can validate what they expected.
fn walk_components(root: &File, relative: &Path, final_directory: bool) -> io::Result<File> {
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
    }
    let mut directory = root.try_clone()?;
    for (index, component) in parts.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "non-relative path",
            ));
        };
        let mut flags = WALK_FLAGS_BASE;
        if index + 1 < parts.len() || final_directory {
            flags |= rustix::fs::OFlags::DIRECTORY;
        }
        directory = File::from(rustix::fs::openat(
            &directory,
            Path::new(name),
            flags,
            rustix::fs::Mode::empty(),
        )?);
    }
    Ok(directory)
}

/// Open a regular file under `root` at a relative path. The nonblocking final
/// open prevents a swapped FIFO from hanging the scanner; the caller's
/// `is_file` check rejects the swap instead of reading it.
pub fn open_regular(root: &File, relative: &Path) -> io::Result<File> {
    let file = walk_components(root, relative, false)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Open an absolute directory by walking descriptors from `/`, so no
/// component can be substituted by a symlink mid-walk.
pub fn open_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::other("absolute directory required"));
    }
    let parts: Vec<_> = path.components().collect();
    walk_from_root(&parts, true)
}

fn walk_from_root(parts: &[std::path::Component<'_>], final_directory: bool) -> io::Result<File> {
    let mut directory = File::from(rustix::fs::openat(
        rustix::fs::CWD,
        Path::new("/"),
        WALK_FLAGS_BASE | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )?);
    for (index, component) in parts.iter().enumerate() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                let mut flags = WALK_FLAGS_BASE;
                if index + 1 < parts.len() || final_directory {
                    flags |= rustix::fs::OFlags::DIRECTORY;
                }
                directory = File::from(rustix::fs::openat(
                    &directory,
                    Path::new(name),
                    flags,
                    rustix::fs::Mode::empty(),
                )?);
            }
            _ => return Err(io::Error::other("invalid directory")),
        }
    }
    Ok(directory)
}

/// A path that stays pinned to an open directory descriptor even if the
/// directory is renamed or its parents are replaced. Linux-only: reading
/// through `/proc/self/fd/<n>`.
pub fn pinned_path(file: &File) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}
