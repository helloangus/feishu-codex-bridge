//! Directory persistence and filesystem boundaries stay offline.
//!
//! The runtime routing scenarios that spawn the fake app-server process live
//! in the test-support package (`tests/directory_routing.rs`).
use bridge_app::{
    directories::DirectoryStore,
    sessions::{PreferenceChange, SessionStore},
};
use bridge_core::SessionKey;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::error::Error;

#[tokio::test]
async fn creation_requires_proposal_and_preserves_directory_on_save_failure()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let state = temp.path().join("state");
    let store = AsyncState::new(JsonStore::open(&state)?);
    let target = store
        .propose_directory(root.clone(), root.clone(), "new parent/child".into())
        .await?
        .ok_or("missing proposal")?;
    assert!(!target.exists());
    assert!(store.directory_preferences().await?.is_empty());
    assert_eq!(
        store
            .create_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "new parent/child".into(),
                target.clone()
            )
            .await?,
        target
    );
    assert!(target.is_dir());
    assert_eq!(
        store.directory_preferences().await?.get("owner"),
        Some(&target)
    );
    // A consumed confirmation cannot create again; existing paths use normal /cd.
    assert!(
        store
            .create_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "new parent/child".into(),
                target.clone()
            )
            .await
            .is_err()
    );
    let next = store
        .propose_directory(root.clone(), root.clone(), "another".into())
        .await?
        .ok_or("missing proposal")?;
    std::fs::create_dir(state.join("state.previous.json"))?;
    assert!(
        store
            .create_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "another".into(),
                next.clone()
            )
            .await
            .is_err()
    );
    assert!(next.is_dir());
    assert_eq!(
        store.directory_preferences().await?.get("owner"),
        Some(&target)
    );
    Ok(())
}

#[tokio::test]
async fn creation_revalidates_proposed_path_and_rejects_symlink_replacement()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let parent = root.join("parent");
    std::fs::create_dir(&parent)?;
    let store = AsyncState::new(JsonStore::open(&temp.path().join("state"))?);
    let workspace = bridge_local::workspace::Workspace::new(&root)?;
    assert!(
        workspace
            .create_confirmed(&root.join("unexpected/../other"))
            .is_err()
    );
    assert!(!root.join("unexpected").exists());
    assert!(
        store
            .propose_directory(root.clone(), root.clone(), "../outside".into())
            .await
            .is_err()
    );
    let target = store
        .propose_directory(root.clone(), root.clone(), "parent/new".into())
        .await?
        .ok_or("missing proposal")?;
    #[cfg(unix)]
    {
        std::fs::rename(&parent, root.join("old-parent"))?;
        std::os::unix::fs::symlink(temp.path(), &parent)?;
        assert!(
            store
                .create_directory(
                    "owner".into(),
                    root.clone(),
                    root.clone(),
                    "parent/new".into(),
                    target.clone()
                )
                .await
                .is_err()
        );
        assert!(!temp.path().join("new").exists());
        // Direct creator also refuses links, independently of proposal validation.
        assert!(
            bridge_local::workspace::Workspace::new(&root)?
                .create_confirmed(&target)
                .is_err()
        );
    }
    assert!(store.directory_preferences().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn directory_change_persists_only_owner_and_preserves_session_settings()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let child = root.join("dir with spaces");
    std::fs::create_dir(&child)?;
    let state = temp.path().join("state");
    let store = AsyncState::new(JsonStore::open(&state)?);
    let key = SessionKey::new("owner", &root);
    store.bind(key.clone(), "old-thread".into()).await?;
    store
        .set_preference(key.clone(), PreferenceChange::Plan(true))
        .await?;
    assert_eq!(
        store
            .change_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "dir with spaces".into()
            )
            .await?,
        child
    );
    assert_eq!(
        store
            .inspect_directory(root.clone(), root.clone())
            .await?
            .children,
        vec!["dir with spaces"]
    );
    assert_eq!(store.thread(key.clone()).await?, Some("old-thread".into()));
    assert!(store.preferences(key).await?.plan);
    drop(store);
    let reopened = AsyncState::new(JsonStore::open(&state)?);
    let saved = reopened.directory_preferences().await?;
    assert_eq!(saved.get("owner"), Some(&child));
    assert!(!saved.contains_key("other"));
    assert_eq!(
        reopened
            .change_directory("owner".into(), root.clone(), child, "..".into())
            .await?,
        root
    );
    Ok(())
}

#[tokio::test]
async fn invalid_paths_and_redirected_snapshots_are_rejected() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("file"), "not a directory")?;
    let store = AsyncState::new(JsonStore::open(&temp.path().join("state"))?);
    for input in ["../", "missing", "file", "bad\nname", ""] {
        assert!(
            store
                .change_directory("owner".into(), root.clone(), root.clone(), input.into())
                .await
                .is_err(),
            "{input}"
        );
    }
    assert!(store.directory_preferences().await?.is_empty());
    assert!(!root.join("missing").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path(), root.join("escape"))?;
        assert!(
            store
                .change_directory("owner".into(), root.clone(), root.clone(), "escape".into())
                .await
                .is_err()
        );
        let child = root.join("child");
        std::fs::create_dir(&child)?;
        store
            .change_directory("owner".into(), root.clone(), root.clone(), "child".into())
            .await?;
        std::fs::rename(&child, root.join("moved"))?;
        std::os::unix::fs::symlink(temp.path(), &child)?;
        assert!(
            store
                .validate_directory(root.clone(), child.clone())
                .await
                .is_err()
        );
        assert!(
            store
                .change_directory("owner".into(), root.clone(), child.clone(), "..".into())
                .await
                .is_err()
        );
        assert_eq!(
            store
                .change_directory(
                    "owner".into(),
                    root.clone(),
                    child,
                    root.to_string_lossy().into_owned()
                )
                .await?,
            root
        );
        assert!(
            !store
                .inspect_directory(root.clone(), root)
                .await?
                .children
                .contains(&"escape".to_owned())
        );
    }
    Ok(())
}

#[tokio::test]
async fn failed_directory_save_keeps_previous_choice() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let child = root.join("child");
    std::fs::create_dir(&child)?;
    let state = temp.path().join("state");
    let store = AsyncState::new(JsonStore::open(&state)?);
    store
        .change_directory("owner".into(), root.clone(), root.clone(), ".".into())
        .await?;
    std::fs::create_dir(state.join("state.previous.json"))?;
    assert!(
        store
            .change_directory("owner".into(), root.clone(), root.clone(), "child".into())
            .await
            .is_err()
    );
    assert_eq!(
        store.directory_preferences().await?.get("owner"),
        Some(&root)
    );
    drop(store);
    let reopened = AsyncState::new(JsonStore::open(&state)?);
    assert_eq!(
        reopened.directory_preferences().await?.get("owner"),
        Some(&root)
    );
    Ok(())
}
