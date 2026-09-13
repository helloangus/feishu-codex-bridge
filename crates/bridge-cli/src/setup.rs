//! Generate an explicit private configuration without credentials or deployment.
use crate::{
    AccessConfig, AccessMode, CodexConfig, Config, FeishuConfig, Sandbox, WorkspaceConfig,
};
use std::{collections::BTreeSet, fs, io::Write, os::unix::fs::PermissionsExt, path::Path};

pub fn initialize(
    output: &Path,
    root: &Path,
    cwd: &Path,
    state: &Path,
    users: BTreeSet<String>,
    unrestricted: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !root.is_absolute() || !cwd.is_absolute() || !state.is_absolute() {
        return Err("工作区、初始目录和状态目录必须为绝对路径".into());
    }
    let users: BTreeSet<String> = users
        .into_iter()
        .map(|user| user.trim().to_owned())
        .collect();
    let config = Config {
        workspace: WorkspaceConfig {
            root: fs::canonicalize(root)?,
            cwd: fs::canonicalize(cwd)?,
            state_dir: state.into(),
        },
        access: AccessConfig {
            mode: AccessMode::Restricted,
            pairing_code_env: if users.is_empty() {
                Some("FEISHU_PAIRING_CODE".into())
            } else {
                None
            },
            allowed_open_ids: users,
        },
        codex: CodexConfig {
            executable: "codex".into(),
            args: vec![
                "app-server".into(),
                "--enable".into(),
                "collaboration_modes".into(),
            ],
            sandbox: if unrestricted {
                Sandbox::DangerFullAccess
            } else {
                Sandbox::WorkspaceWrite
            },
        },
        feishu: Some(FeishuConfig {
            app_id_env: "FEISHU_APP_ID".into(),
            app_secret_env: "FEISHU_APP_SECRET".into(),
            python: None,
            adapter: None,
            proxy_env: None,
        }),
    };
    config.validate()?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    pending
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    pending.write_all(toml::to_string_pretty(&config)?.as_bytes())?;
    pending.as_file().sync_all()?;
    pending.persist_noclobber(output)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_configuration_preserves_paths_and_refuses_overwrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("project with spaces");
        fs::create_dir(&root)?;
        let output = temp.path().join("bridge.toml");
        let state = temp.path().join("state");
        initialize(&output, &root, &root, &state, BTreeSet::new(), false)?;
        let config = Config::read(&output)?;
        config.validate()?;
        assert_eq!(config.workspace.root, root);
        assert!(matches!(config.codex.sandbox, Sandbox::WorkspaceWrite));
        assert_eq!(
            config.access.pairing_code_env.as_deref(),
            Some("FEISHU_PAIRING_CODE")
        );
        assert_eq!(fs::metadata(&output)?.permissions().mode() & 0o777, 0o600);
        let before = fs::read(&output)?;
        assert!(initialize(&output, &root, &root, &state, BTreeSet::new(), true).is_err());
        assert_eq!(fs::read(&output)?, before);
        assert!(!state.exists());
        Ok(())
    }
    #[test]
    fn invalid_workspace_never_publishes_configuration() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("root");
        fs::create_dir(&root)?;
        let output = temp.path().join("bridge.toml");
        assert!(
            initialize(
                &output,
                &root,
                temp.path(),
                &temp.path().join("state"),
                BTreeSet::new(),
                false
            )
            .is_err()
        );
        assert!(!output.exists());
        initialize(
            &output,
            &root,
            &root,
            &temp.path().join("state"),
            BTreeSet::from(["ou_test".into()]),
            true,
        )?;
        let config = Config::read(&output)?;
        assert!(config.access.pairing_code_env.is_none());
        assert!(matches!(config.codex.sandbox, Sandbox::DangerFullAccess));
        Ok(())
    }
}
