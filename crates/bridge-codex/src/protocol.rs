//! Versioned Codex protocol snapshot shared by production mappings and tests.
//!
//! Schemas and fixtures below pin the exact wire shapes of one Codex CLI
//! release. Upgrading the snapshot is an explicit maintenance step recorded in
//! `manifest.json`; ordinary tests and CI only read these files offline.

use std::path::PathBuf;

/// Codex CLI release whose app-server protocol the vendored snapshot pins.
///
/// Schema files live under [`schema_dir`] and recorded fixtures under
/// [`fixture_dir`]; `manifest.json` inside the schema directory must keep
/// declaring this version as its provenance.
pub const CODEX_SCHEMA_BASELINE: &str = "0.153.4";

/// Repository-relative root of the versioned schema snapshots.
pub const SCHEMA_ROOT: &str = "schemas/codex";

/// Repository-relative root of the recorded protocol fixtures.
pub const FIXTURE_ROOT: &str = "fixtures/codex";

/// Absolute schema directory for [`CODEX_SCHEMA_BASELINE`], resolved from this
/// crate's manifest path so tests never depend on the process working directory.
pub fn schema_dir() -> PathBuf {
    workspace_root()
        .join(SCHEMA_ROOT)
        .join(CODEX_SCHEMA_BASELINE)
}

/// Absolute fixtures directory for [`CODEX_SCHEMA_BASELINE`], resolved like [`schema_dir`].
pub fn fixture_dir() -> PathBuf {
    workspace_root()
        .join(FIXTURE_ROOT)
        .join(CODEX_SCHEMA_BASELINE)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[cfg(test)]
pub(crate) mod testing {
    //! Offline schema validation helpers for in-crate unit tests.

    use crate::protocol::schema_dir;
    use serde_json::Value;
    use std::{error::Error, fs};

    /// Read one schema file from the pinned snapshot by file name.
    pub(crate) fn read(name: &str) -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_slice(&fs::read(schema_dir().join(name))?)?)
    }

    /// Assert that `instance` validates against the named pinned schema file.
    pub(crate) fn validate(name: &str, instance: &Value) -> Result<(), Box<dyn Error>> {
        let schema = read(name)?;
        let validator = jsonschema::validator_for(&schema)?;
        validator
            .validate(instance)
            .map_err(|error| error.to_string().into())
    }
}
