use clap::Parser;
use tiv_cli::{
    Cli, Command, DoctorArgs, ReferenceAppCaseArgs, ReferenceAppEvidenceArgs,
    ReferenceAppReplayArgs, ReplayCommand, TraceCommand,
};

#[test]
fn the_cli_exposes_doctor_with_a_safe_default_config_path() {
    let default = Cli::try_parse_from(["tiv", "doctor"]).expect("the doctor command parses");
    assert_eq!(
        default.command,
        Command::Doctor(DoctorArgs {
            config: "tiv.toml".into(),
        })
    );

    let explicit = Cli::try_parse_from(["tiv", "doctor", "--config", "safe/tiv.toml"])
        .expect("the explicit doctor config parses");
    assert_eq!(
        explicit.command,
        Command::Doctor(DoctorArgs {
            config: "safe/tiv.toml".into(),
        })
    );
}

#[test]
fn the_cli_exposes_init_without_an_implicit_overwrite_flag() {
    let cli = Cli::try_parse_from(["tiv", "init"]).expect("the init command parses");
    assert_eq!(cli.command, Command::Init);
    assert!(Cli::try_parse_from(["tiv", "init", "--force"]).is_err());
}

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
                webhook_timestamp: None,
            }),
        }
    );
}

#[test]
fn the_cli_exposes_a_self_contained_reference_app_evidence_run() {
    let cli = Cli::try_parse_from([
        "tiv",
        "replay",
        "reference-app-evidence",
        "--trace",
        "compiled-trace.json",
        "--postgres-port",
        "15432",
        "--reference-app-url",
        "http://127.0.0.1:18080",
        "--fixture-control-url",
        "http://127.0.0.1:12112",
    ])
    .expect("the self-contained evidence command parses");

    assert_eq!(
        cli.command,
        Command::Replay {
            command: ReplayCommand::ReferenceAppEvidence(ReferenceAppEvidenceArgs {
                trace: "compiled-trace.json".into(),
                postgres_port: 15_432,
                postgres_admin_role: "tiv_admin".to_owned(),
                reference_app_url: "http://127.0.0.1:18080".to_owned(),
                fixture_control_url: "http://127.0.0.1:12112".to_owned(),
            }),
        }
    );
}

#[test]
fn the_cli_exposes_an_attested_planned_reference_case_run() {
    let cli = Cli::try_parse_from([
        "tiv",
        "replay",
        "reference-app-case",
        "--plan",
        "planned-case.json",
        "--journal",
        "artifacts/case.jsonl",
        "--postgres-port",
        "15432",
        "--reference-app-url",
        "http://127.0.0.1:18080",
        "--fixture-control-url",
        "http://127.0.0.1:12112",
    ])
    .expect("the planned reference case command parses");

    assert_eq!(
        cli.command,
        Command::Replay {
            command: ReplayCommand::ReferenceAppCase(ReferenceAppCaseArgs {
                plan: "planned-case.json".into(),
                journal: "artifacts/case.jsonl".into(),
                postgres_port: 15_432,
                postgres_admin_role: "tiv_admin".to_owned(),
                reference_app_url: "http://127.0.0.1:18080".to_owned(),
                fixture_control_url: "http://127.0.0.1:12112".to_owned(),
            }),
        }
    );
}

#[test]
fn unknown_commands_are_rejected_by_the_typed_surface() {
    assert!(Cli::try_parse_from(["tiv", "run-production"]).is_err());
}
