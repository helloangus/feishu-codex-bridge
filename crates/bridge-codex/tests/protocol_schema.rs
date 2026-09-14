use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{error::Error, fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_json(path: PathBuf) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn validate(schema: &Value, instance: &Value) -> Result<(), Box<dyn Error>> {
    let validator = jsonschema::validator_for(schema)?;
    validator
        .validate(instance)
        .map_err(|error| error.to_string().into())
}

#[test]
fn generated_schema_matches_provenance_manifest() -> Result<(), Box<dyn Error>> {
    let schema = root().join("schemas/codex/0.153.4");
    let manifest = read_json(schema.join("manifest.json"))?;
    assert_eq!(manifest["cli_version"], "0.153.4");
    for (name, digest) in manifest["files"].as_object().ok_or("invalid manifest")? {
        assert_eq!(
            format!("{:x}", Sha256::digest(fs::read(schema.join(name))?)),
            digest.as_str().ok_or("invalid digest")?
        );
    }
    Ok(())
}

#[test]
fn turn_fixtures_match_versioned_schema() -> Result<(), Box<dyn Error>> {
    let schema = read_json(root().join("schemas/codex/0.153.4/TurnStartParams.json"))?;
    for name in ["turn-start-plan.json", "turn-start-default.json"] {
        let mut payload = read_json(root().join("fixtures/codex/0.153.4").join(name))?;
        validate(&schema, &payload)?;
        assert_eq!(payload["approvalPolicy"], "on-request");
        assert_eq!(payload["sandboxPolicy"]["networkAccess"], false);
        assert_eq!(payload["sandboxPolicy"]["writableRoots"][0], payload["cwd"]);
        payload["collaborationMode"]["settings"]
            .as_object_mut()
            .ok_or("missing settings")?
            .remove("model");
        assert!(validate(&schema, &payload).is_err());
    }
    Ok(())
}

#[test]
fn request_and_reply_fixtures_match_versioned_schemas() -> Result<(), Box<dyn Error>> {
    let cases = read_json(root().join("fixtures/codex/0.153.4/server-requests.json"))?;
    for case in cases.as_array().ok_or("invalid cases")? {
        for (suffix, field) in [("Params", "params"), ("Response", "reply")] {
            let name = format!(
                "{}{}.json",
                case["schema"].as_str().ok_or("schema")?,
                suffix
            );
            let schema = read_json(root().join("schemas/codex/0.153.4").join(name))?;
            validate(&schema, &case[field])?;
        }
    }
    Ok(())
}
