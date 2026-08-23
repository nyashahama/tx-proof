use std::process::ExitCode;

use clap::Parser;
use tiv_cli::{Cli, Command, execute_async, execute_async_with_cancellation};
use tiv_runtime::configured_campaign::RunCancellation;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = if matches!(&cli.command, Command::Run(_)) {
        let cancellation = RunCancellation::new();
        let signal_cancellation = cancellation.clone();
        let mut signal_task = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_cancellation.cancel();
            }
        });
        let result = execute_async_with_cancellation(cli, &cancellation).await;
        signal_task.abort();
        let _ = (&mut signal_task).await;
        result
    } else {
        execute_async(cli).await
    };
    match result {
        Ok(output) => {
            println!("{}", output.body());
            ExitCode::from(output.exit_code())
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}
