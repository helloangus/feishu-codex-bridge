//! Shared offline test infrastructure for the bridge workspace.
//!
//! This package concentrates the pieces that used to be copied between crate
//! test suites: fake Codex processes (same-package binaries, addressable via
//! `CARGO_BIN_EXE_*`), runtime `Input` construction, a recording `Messenger`
//! port fake, and a harness that binds a fake app-server process to the real
//! serial runtime. Everything runs offline against temporary directories.

pub mod actor;
pub mod diagnostics;
pub mod fakes;
pub mod files;
pub mod input;
pub mod messenger;
