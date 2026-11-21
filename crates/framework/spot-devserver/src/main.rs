//! The `spot-devserver` binary: a thin wrapper over [`spot_devserver::cli`].

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use spot_devserver::cli::{DevArgs, run};

fn main() -> ExitCode {
    let args = match DevArgs::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return ExitCode::from(u8::try_from(code).unwrap_or(2));
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spot-devserver: {}: {error}", error.code());
            ExitCode::FAILURE
        }
    }
}
