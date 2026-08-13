use std::process::ExitCode;

use clap::Parser;
use tiv_cli::{Cli, execute_async};

#[tokio::main]
async fn main() -> ExitCode {
    match execute_async(Cli::parse()).await {
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
