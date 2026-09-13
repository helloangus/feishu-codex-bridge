use bridge_cli::FeishuConfig;

#[test]
fn native_configuration_requires_no_python_and_accepts_legacy_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let native: FeishuConfig = toml::from_str("app_id_env='APP_ID'\napp_secret_env='APP_SECRET'")?;
    assert!(native.python.is_none() && native.adapter.is_none());
    let legacy: FeishuConfig = toml::from_str(
        "app_id_env='APP_ID'\napp_secret_env='APP_SECRET'\npython='/missing/python'\nadapter='/missing/adapter.py'",
    )?;
    assert!(legacy.python.is_some() && legacy.adapter.is_some());
    Ok(())
}
