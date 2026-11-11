//! The `spot-scaffold` command line. See [`spot_scaffold::cli`].

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use spot_scaffold::cli::{Cli, run};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(u8::try_from(code).unwrap_or(2));
        }
    };
    ExitCode::from(run(&cli))
}
