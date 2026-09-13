//! Resolve against an explicit root without changing the process directory.
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "工作区根不是目录",
            ));
        }
        Ok(Self { root })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Walk directory descriptors without following substituted symlinks.
    /// A failure may leave already-created parents; never delete them implicitly.
    pub fn create_confirmed(&self, target: &Path) -> io::Result<()> {
        use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
        let relative = target
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "目录越出工作区"))?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "非规范目录"));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        // Open every root component as well, not just its final component.
        let mut directory = fs::File::from(openat(CWD, Path::new("/"), flags, Mode::empty())?);
        for component in self.root.components() {
            let Component::Normal(name) = component else {
                if component == Component::RootDir {
                    continue;
                }
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "非规范目录"));
            };
            directory = fs::File::from(openat(&directory, Path::new(name), flags, Mode::empty())?);
        }
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "非规范目录"));
            };
            let name = Path::new(name);
            let next = match openat(&directory, name, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => {
                    match mkdirat(&directory, name, Mode::RWXU) {
                        Ok(()) => directory.sync_all()?,
                        Err(rustix::io::Errno::EXIST) => {}
                        Err(error) => return Err(error.into()),
                    }
                    openat(&directory, name, flags, Mode::empty())?
                }
                Err(error) => return Err(error.into()),
            };
            directory = fs::File::from(next);
        }
        Ok(())
    }
    pub fn resolve_existing(&self, current: &Path, input: &Path) -> io::Result<PathBuf> {
        let path = fs::canonicalize(if input.is_absolute() {
            input.to_path_buf()
        } else {
            current.join(input)
        })?;
        if !path.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "目录越出工作区",
            ));
        }
        if !path.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "目标不是目录"));
        }
        Ok(path)
    }
    /// Creation is not performed here. Re-resolve again when a user confirms.
    pub fn resolve_proposed(&self, current: &Path, input: &Path) -> io::Result<PathBuf> {
        let full = if input.is_absolute() {
            input.to_owned()
        } else {
            current.join(input)
        };
        let mut prefix = PathBuf::new();
        for component in full.components() {
            match component {
                Component::ParentDir => {
                    prefix.pop();
                }
                Component::CurDir => {}
                other => prefix.push(other),
            }
            match fs::symlink_metadata(&prefix) {
                Ok(_) => {
                    prefix = fs::canonicalize(&prefix)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if !prefix.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "目录越出工作区",
            ));
        }
        Ok(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_links_stay_in_workspace() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        let workspace = Workspace::new(&root)?;
        assert_eq!(
            workspace.resolve_proposed(&root, Path::new("new/child"))?,
            root.join("new/child")
        );
        assert!(
            workspace
                .resolve_proposed(&root, Path::new("../outside/new"))
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path(), root.join("outside"))?;
            assert!(
                workspace
                    .resolve_existing(&root, Path::new("outside"))
                    .is_err()
            );
            assert!(
                workspace
                    .resolve_proposed(&root, Path::new("outside/new"))
                    .is_err()
            );
            std::os::unix::fs::symlink(root.join("missing"), root.join("broken"))?;
            assert!(
                workspace
                    .resolve_proposed(&root, Path::new("broken/new"))
                    .is_err()
            );
        }
        Ok(())
    }
}
