//! Linux-only supervision: adopt descendants before launching a bridge attempt.
use std::{io, time::Duration};

pub fn enable() -> io::Result<()> {
    let pid = rustix::process::Pid::from_raw(
        i32::try_from(std::process::id()).map_err(io::Error::other)?,
    )
    .ok_or_else(|| io::Error::other("invalid self pid"))?;
    let _probe = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
    children()?;
    rustix::process::set_child_subreaper(rustix::process::Pid::from_raw(1))?;
    Ok(())
}

fn children() -> io::Result<Vec<rustix::process::Pid>> {
    let mut children = std::collections::BTreeSet::new();
    let mut missing_children = false;
    for task in std::fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing_children = true;
                continue;
            }
            Err(e) => return Err(e),
        };
        for value in text.split_whitespace() {
            let raw = value.parse::<i32>().map_err(io::Error::other)?;
            if raw > 1 {
                children.insert(raw);
            }
        }
    }
    if missing_children {
        return children_from_status(std::path::Path::new("/proc"), std::process::id());
    }
    Ok(children
        .into_iter()
        .filter_map(rustix::process::Pid::from_raw)
        .collect())
}

fn parent_pid(status: &str) -> io::Result<u32> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .ok_or_else(|| io::Error::other("proc status has no parent PID"))?
        .trim()
        .parse::<u32>()
        .map_err(io::Error::other)
}

// PRoot may expose status/PPid while omitting task/*/children. Do not
// interpret that omission as an empty process tree.
fn children_from_status(
    root: &std::path::Path,
    owner: u32,
) -> io::Result<Vec<rustix::process::Pid>> {
    let mut children = Vec::new();
    let mut owner_visible = false;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        let status = match std::fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            // An unreadable entry may hide a child: fail rather than claim
            // that cleanup has completed. Target Android permissions need validation.
            Err(e) => return Err(e),
        };
        if pid as u32 == owner {
            owner_visible = true;
        }
        if parent_pid(&status)? == owner {
            if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                children.push(pid);
            }
        }
    }
    if !owner_visible {
        return Err(io::Error::other(
            "own proc status is not visible; cannot confirm cleanup",
        ));
    }
    Ok(children)
}

/// Called only after the owned bridge has been reaped, before another attempt.
pub async fn clean() -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let children = children()?;
        if children.is_empty() {
            return Ok(());
        }
        for pid in children {
            // An open pidfd cannot redirect a signal if the numeric PID is reused.
            let fd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::SRCH) => continue,
                Err(e) => return Err(e.into()),
            };
            // Recheck ownership after opening the descriptor.
            if !self::children()?.contains(&pid) {
                continue;
            }
            match rustix::process::pidfd_send_signal(&fd, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                Err(e) => return Err(e.into()),
            }
            match rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG) {
                Ok(_) | Err(rustix::io::Errno::CHILD) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::other(
                "descendant cleanup incomplete; refusing restart",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_fallback_selects_only_owned_children_and_requires_owner() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        for (pid, parent) in [(101, 1), (102, 101), (103, 99)] {
            let path = tmp.path().join(pid.to_string());
            std::fs::create_dir(&path)?;
            std::fs::write(
                path.join("status"),
                format!("Name:\tfake\nPPid:\t{parent}\n"),
            )?;
        }
        let result = children_from_status(tmp.path(), 101)?;
        assert_eq!(
            result
                .iter()
                .map(|pid| pid.as_raw_nonzero().get())
                .collect::<Vec<_>>(),
            vec![102]
        );
        assert!(children_from_status(tmp.path(), 999).is_err());
        assert!(parent_pid("Name:\tmissing").is_err());
        Ok(())
    }
    #[test]
    fn cleanup_real_child_in_isolated_helper() -> io::Result<()> {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "descendants::tests::cleanup_helper",
                "--test-threads=1",
            ])
            .env("BRIDGE_CLEANUP_HELPER", "1")
            .status()?;
        assert!(status.success());
        Ok(())
    }
    #[test]
    fn cleanup_helper() -> io::Result<()> {
        if std::env::var_os("BRIDGE_CLEANUP_HELPER").is_none() {
            return Ok(());
        }
        enable()?;
        let mut child = std::process::Command::new("sleep").arg("3").spawn()?;
        let result = (|| -> io::Result<()> {
            let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
            assert!(children()?.iter().any(|p| p.as_raw_nonzero().get() == pid));
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(clean())?;
            assert!(children()?.is_empty());
            Ok(())
        })();
        // clean() may already have reaped this child; always attempt cleanup
        // on an error too, without requiring a second wait to succeed.
        let _ = child.kill();
        let _ = child.wait();
        result?;
        Ok(())
    }
}
