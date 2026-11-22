//! The `bohio` command: arguments, output and the diagnostics subscriber; everything else is the
//! library.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    bohio::cli::main()
}
