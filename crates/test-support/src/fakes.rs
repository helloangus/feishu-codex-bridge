//! Absolute paths of the fake Codex processes built with this package.
//!
//! `CARGO_BIN_EXE_<name>` is only defined while cargo compiles the
//! integration tests of the package that owns the binary, so the paths are
//! exposed as macros that the caller expands inside its own test crate.
//! Tests never build anything themselves: cargo builds both fakes before it
//! runs this package's test targets.

/// Absolute path of the minimal protocol fake (`fake-codex-process`).
///
/// Expands `env!("CARGO_BIN_EXE_fake-codex-process")`; therefore only usable
/// inside test-support's own integration tests.
#[macro_export]
macro_rules! fake_codex_process {
    () => {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-codex-process"))
    };
}

/// Absolute path of the scripted app-server fake (`fake-codex-runtime`).
///
/// Expands `env!("CARGO_BIN_EXE_fake-codex-runtime")`; therefore only usable
/// inside test-support's own integration tests.
#[macro_export]
macro_rules! fake_codex_runtime {
    () => {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-codex-runtime"))
    };
}
