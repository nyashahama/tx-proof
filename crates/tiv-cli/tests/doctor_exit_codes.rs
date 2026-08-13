use tiv_cli::CliError;
use tiv_runtime::doctor::DoctorError;

#[test]
fn doctor_safety_and_infrastructure_failures_have_non_overlapping_exit_codes() {
    assert_eq!(
        CliError::Doctor(DoctorError::LiveStripeMaterial).exit_code(),
        2
    );
    assert_eq!(
        CliError::Doctor(DoctorError::DockerUnavailable).exit_code(),
        3
    );
}

#[test]
fn the_binary_preserves_the_doctor_exit_contract() {
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    let invalid = std::process::Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args(["doctor", "--config", "definitely-missing-tiv.toml"])
        .output()
        .expect("the tiv binary executes");
    assert_eq!(invalid.status.code(), Some(2));

    let infrastructure = std::process::Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args(["doctor", "--config", config_path])
        .env(
            "TIV_POSTGRES_ADMIN_URL",
            "postgresql://tiv_admin:canary@127.0.0.1:15432/postgres",
        )
        .env(
            "DATABASE_URL",
            "postgresql://tiv_app:canary@127.0.0.1:15432/tiv_case_checkout",
        )
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_canary")
        .env("PATH", "")
        .output()
        .expect("the tiv binary executes without PATH");
    assert_eq!(infrastructure.status.code(), Some(3));
}
