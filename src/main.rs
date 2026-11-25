//! Binary entry point; see the library crate for the implementation.

use std::error::Error as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    match simple_bar::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprint!("simple-bar: {err}");
            let mut source = err.source();
            while let Some(cause) = source {
                eprint!(": {cause}");
                source = cause.source();
            }
            eprintln!();
            ExitCode::FAILURE
        }
    }
}
