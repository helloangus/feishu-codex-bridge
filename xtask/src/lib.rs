//! xtask: repository tooling (packaging, verification, boundary gates).
//!
//! Binaries are thin; all logic lives here so integration tests can exercise
//! the same code paths without running cargo recursively.
pub mod boundaries;
pub mod cargo_cli;
pub mod check;
pub mod codex_schema;
pub mod hygiene;
pub mod package;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check [--fetch] [--offline]
                            run fmt, clippy, workspace tests (serial), product
                            debug build, rustdoc (warnings as errors), codex
                            protocol snapshot verification, dependency
                            boundaries, repository hygiene, shell syntax,
                            git diff --check and packaging structure checks
  check-boundaries          dependency boundary gate (same check as in 'check')
  fetch                     fetch locked dependencies for offline check runs
  package [--release] [--allow-dirty] [OUTPUT_DIR]
                            build the host release binary and publish a verified
                            package (bridge, bridge.example.toml, DEPLOYMENT.md,
                            BUILD-INFO.txt, SHA256SUMS); default dir: dist/
  codex-schema <subcommand>
                            pinned Codex protocol snapshot maintenance:
                            check (offline), export (explicit, --codex <path>)
  help                      print this message";

fn print_usage() {
    println!("{USAGE}");
}

/// Dispatch a parsed argument list (without the program name).
pub fn run<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<String> = args.into_iter().collect();
    match args.first().map(String::as_str) {
        None => {
            print_usage();
            Err("missing command".to_string())
        }
        Some("help" | "--help" | "-h") => {
            print_usage();
            Ok(())
        }
        Some("check") => check::run_command(&args[1..]),
        Some("check-boundaries") => boundaries::run_command(),
        Some("fetch") => check::run_fetch_command(&args[1..]),
        Some("package") => package::run_command(&args[1..]),
        Some("codex-schema") => codex_schema::run_command(&args[1..]),
        Some(other) => {
            print_usage();
            Err(format!("unknown command '{other}'"))
        }
    }
}
