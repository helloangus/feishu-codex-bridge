//! The per-instance application lock excludes any second runtime instance.
use bridge_cli::bootstrap::app_lock;
use std::error::Error;

#[test]
fn global_app_lock_excludes_second_instance() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let lock = app_lock(temp.path(), "test-app")?;
    assert!(app_lock(temp.path(), "test-app").is_err());
    drop(lock);
    assert!(app_lock(temp.path(), "test-app").is_ok());
    Ok(())
}
