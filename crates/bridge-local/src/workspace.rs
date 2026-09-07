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
