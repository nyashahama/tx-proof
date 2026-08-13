use tiv_core::trace::CompiledTrace;
use tiv_runtime::{
    reference_app::{
        ReferenceAppEvidenceConfig, ReferenceAppEvidenceConfigError, run_reference_app_evidence,
    },
    replay::{ReferenceAppReplayConfigError, ReplayPlan},
};

#[tokio::test]
async fn evidence_config_rejects_non_loopback_targets_before_execution() {
    let result = ReferenceAppEvidenceConfig::attest(
        15_432,
        "tiv_admin",
        "admin-local-only-password",
        "app-local-only-password",
        "https://example.com",
        "http://127.0.0.1:12112",
        "run-scoped-control-token",
    )
    .await;

    assert!(matches!(
        result,
        Err(ReferenceAppEvidenceConfigError::Replay(
            ReferenceAppReplayConfigError::NonLoopbackUrl
        ))
    ));
}

#[tokio::test]
async fn evidence_config_rejects_invalid_postgres_credentials_before_execution() {
    let result = ReferenceAppEvidenceConfig::attest(
        0,
        "tiv_admin",
        "",
        "app-local-only-password",
        "http://127.0.0.1:18080",
        "http://127.0.0.1:12112",
        "run-scoped-control-token",
    )
    .await;

    assert!(matches!(
        result,
        Err(ReferenceAppEvidenceConfigError::InvalidPostgresConfiguration)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the isolated reference-app Compose project"]
async fn evidence_run_provisions_resets_and_replays_the_same_failure_identity() {
    let postgres_port = std::env::var("TIV_POSTGRES_TEST_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(15_432);
    let reference_app_url = std::env::var("TIV_REFERENCE_APP_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:18080".to_owned());
    let fixture_control_url = std::env::var("TIV_FIXTURE_CONTROL_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:12112".to_owned());
    let config = ReferenceAppEvidenceConfig::attest(
        postgres_port,
        "tiv_admin",
        "tiv-local-only-password",
        "tiv-app-local-only-password",
        reference_app_url,
        fixture_control_url,
        "run-scoped-control-token",
    )
    .await
    .expect("the isolated evidence configuration is valid");
    let trace: CompiledTrace =
        serde_json::from_str(include_str!("../../../spike/compiled-trace-v1.json"))
            .expect("the committed trace is valid");
    let plan = ReplayPlan::from_trace(&trace).expect("the committed trace is executable");

    let evidence = run_reference_app_evidence(&plan, &config)
        .await
        .expect("the two-leg reference replay emits coherent evidence");
    let encoded = evidence
        .to_pretty_json()
        .expect("the bounded evidence serializes");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("the evidence is JSON");

    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["provider_object_count"], 2);
    assert_eq!(
        value["failure_identity"]["invariant_id"],
        "provider-object-unique"
    );
    assert_eq!(value["fresh_replay_same_identity"], true);
    assert_ne!(
        value["database_reset"]["before_oid"],
        value["database_reset"]["after_oid"]
    );
    assert!(!encoded.contains("run-scoped-control-token"));
    assert!(!encoded.contains("tiv-app-local-only-password"));
}
