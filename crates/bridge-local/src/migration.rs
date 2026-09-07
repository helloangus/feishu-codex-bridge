//! Explicit offline Python-state import/export. Never read or execute .env.
use crate::state::{State, StoreError, atomic_json};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct LegacyPaths {
    pub sessions: PathBuf,
    pub settings: PathBuf,
    pub seen: PathBuf,
    pub allowed: PathBuf,
}
impl LegacyPaths {
    pub fn in_directory(root: &Path) -> Self {
        Self {
            sessions: root.join(".feishu-codex-session"),
            settings: root.join(".feishu-codex-settings"),
            seen: root.join(".feishu-codex-seen-messages"),
            allowed: root.join(".feishu-codex-allowed-open-ids"),
        }
    }
}

#[derive(Debug)]
pub struct ImportedState {
    pub state: State,
    pub seen: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    models: BTreeMap<String, String>,
    directories: BTreeMap<String, String>,
    plan_modes: BTreeMap<String, bool>,
}

fn optional_text(path: &Path) -> Result<Option<String>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn read_legacy(
    paths: &LegacyPaths,
    single_session_key: Option<&str>,
) -> Result<ImportedState, StoreError> {
    let mut state = State::empty();
    if let Some(text) = optional_text(&paths.sessions)? {
        let raw = text.trim();
        if !raw.is_empty() {
            if raw.starts_with('{') || raw.starts_with('[') || raw.starts_with('"') {
                state.sessions = serde_json::from_str(raw)?;
            } else {
                // Legacy thread IDs are opaque, but must not swallow malformed JSON.
                if raw.chars().any(char::is_whitespace) || raw.contains(['{', '}', '[', ']']) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "旧会话格式无效").into());
                }
                let key = single_session_key
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "单会话文件需要 --legacy-user 与 --legacy-cwd",
                        )
                    })?;
                state.sessions.insert(key.into(), raw.into());
            }
        }
    }
    if let Some(text) = optional_text(&paths.settings)? {
        let settings: Settings = serde_json::from_str(&text)?;
        state.models = settings.models;
        state.directories = settings.directories;
        state.plan_modes = settings.plan_modes;
    }
    if let Some(text) = optional_text(&paths.allowed)? {
        state.allowed_open_ids = serde_json::from_str::<BTreeSet<String>>(&text)?;
    }
    let seen = optional_text(&paths.seen)?
        .map(|text| serde_json::from_str(&text))
        .transpose()?
        .unwrap_or_default();
    Ok(ImportedState { state, seen })
}

/// Destination must be new, so rollback exports cannot overwrite live state.
pub fn export_legacy(root: &Path, state: &State, seen: &[String]) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(root)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(root)?;
    let paths = LegacyPaths::in_directory(root);
    atomic_json(&paths.sessions, &state.sessions)?;
    atomic_json(
        &paths.settings,
        &Settings {
            models: state.models.clone(),
            directories: state.directories.clone(),
            plan_modes: state.plan_modes.clone(),
        },
    )?;
    atomic_json(&paths.allowed, &state.allowed_open_ids)?;
    atomic_json(&paths.seen, &seen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_round_trip_and_export_refuses_overwrite() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut state = State::empty();
        state
            .sessions
            .insert("user:/tmp/project".into(), "thread".into());
        state
            .models
            .insert("user:/tmp/project".into(), "model".into());
        state
            .directories
            .insert("user".into(), "/tmp/project".into());
        state.plan_modes.insert("user:/tmp/project".into(), true);
        state.allowed_open_ids.insert("user".into());
        let root = temp.path().join("export");
        export_legacy(&root, &state, &["m".into()])?;
        let restored = read_legacy(&LegacyPaths::in_directory(&root), None)?;
        assert_eq!(restored.state, state);
        assert_eq!(restored.seen, ["m"]);
        assert!(export_legacy(&root, &State::empty(), &[]).is_err());
        Ok(())
    }
    #[test]
    fn single_id_needs_explicit_owner_and_corruption_is_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = LegacyPaths::in_directory(temp.path());
        fs::write(&paths.sessions, "legacy-thread\n")?;
        assert!(read_legacy(&paths, None).is_err());
        assert_eq!(
            read_legacy(&paths, Some("u:/project"))?.state.sessions["u:/project"],
            "legacy-thread"
        );
        fs::write(&paths.sessions, "{broken")?;
        assert!(read_legacy(&paths, Some("u:/project")).is_err());
        Ok(())
    }
}
