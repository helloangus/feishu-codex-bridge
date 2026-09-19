//! Thin binary wrapper; logic lives in the xtask library.
use std::process::ExitCode;

fn main() -> ExitCode {
    match xtask::run(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
