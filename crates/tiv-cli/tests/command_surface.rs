use clap::Parser;
use tiv_cli::{Cli, Command, ReplayCommand, TraceCommand};

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
fn unknown_commands_are_rejected_by_the_typed_surface() {
    assert!(Cli::try_parse_from(["tiv", "run-production"]).is_err());
}
