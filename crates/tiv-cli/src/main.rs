use std::process::ExitCode;

use clap::Parser;
use tiv_cli::{Cli, execute};

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}
