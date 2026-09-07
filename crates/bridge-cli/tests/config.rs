use bridge_cli::Config;
use std::fs;

#[test]
fn restricted_config_requires_explicit_access_and_workspace()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config = temp.path().join("bridge.toml");
    let text = format!(
        "[workspace]\nroot = {:?}\ncwd = {:?}\nstate_dir = {:?}\n[access]\nmode = 'restricted'\nallowed_open_ids = []\n[codex]\nexecutable = 'codex'\nargs = ['app-server']\nsandbox = 'workspaceWrite'\n",
        temp.path(),
        temp.path(),
        temp.path().join("state")
    );
    fs::write(&config, &text)?;
    assert!(Config::read(&config)?.validate().is_err());
    fs::write(
        &config,
        text.replace("allowed_open_ids = []", "allowed_open_ids = ['test-user']"),
    )?;
    Config::read(&config)?.validate()?;
    assert!(!temp.path().join("state").exists());
    fs::write(
        &config,
        text.replace("mode = 'restricted'", "mode = 'open'"),
    )?;
    Config::read(&config)?.validate()?;
    fs::write(&config, text.replace("workspaceWrite", "invalid"))?;
    assert!(Config::read(&config).is_err());
    Ok(())
}
