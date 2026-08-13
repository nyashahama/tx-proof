use clap::Parser;
use tiv_cli::{Cli, Command, ReferenceAppReplayArgs, ReplayCommand, TraceCommand};

#[test]
fn the_cli_exposes_only_the_narrow_trace_validation_command_for_this_slice() {
    let cli = Cli::try_parse_from(["tiv", "trace", "validate", "compiled-trace.json"])
        .expect("the documented command parses");

    assert_eq!(
        cli.command,
        Command::Trace {
            command: TraceCommand::Validate {
                path: "compiled-trace.json".into(),
            },
        }
    );
}

#[test]
fn the_cli_exposes_read_only_replay_inspection_for_compiled_traces() {
    let cli = Cli::try_parse_from(["tiv", "replay", "inspect", "compiled-trace.json"])
        .expect("the documented replay inspection command parses");

    assert_eq!(
        cli.command,
        Command::Replay {
            command: ReplayCommand::Inspect {
                path: "compiled-trace.json".into(),
            },
        }
    );
}

#[test]
fn the_cli_exposes_reference_app_replay_execution_for_prepared_case_databases() {
    let cli = Cli::try_parse_from([
        "tiv",
        "replay",
        "reference-app",
        "--trace",
        "compiled-trace.json",
        "--case-database",
        "tiv_case_7dc6fb6e",
        "--reference-app-url",
        "http://127.0.0.1:18080",
        "--fixture-control-url",
        "http://127.0.0.1:12112",
        "--fixture-control-token",
        "run-scoped-control-token",
        "--reset-sequence",
        "1",
        "--confirm-sequence",
        "2",
    ])
    .expect("the bounded reference-app replay command parses");

    assert_eq!(
        cli.command,
        Command::Replay {
            command: ReplayCommand::ReferenceApp(ReferenceAppReplayArgs {
                trace: "compiled-trace.json".into(),
                case_database: "tiv_case_7dc6fb6e".to_owned(),
                reference_app_url: "http://127.0.0.1:18080".to_owned(),
                fixture_control_url: "http://127.0.0.1:12112".to_owned(),
                fixture_control_token: "run-scoped-control-token".to_owned(),
                reset_sequence: 1,
                confirm_sequence: 2,
                webhook_timestamp: 1_700_000_000,
            }),
        }
    );
}

#[test]
fn unknown_commands_are_rejected_by_the_typed_surface() {
    assert!(Cli::try_parse_from(["tiv", "run-production"]).is_err());
}
