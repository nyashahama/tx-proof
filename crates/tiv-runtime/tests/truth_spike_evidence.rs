use tiv_core::result::{CheckpointId, FailureIdentity, InvariantId};
use tiv_runtime::evidence::{EvidenceError, TruthSpikeEvidence};

#[test]
fn evidence_records_the_safety_and_replay_facts_without_secrets() {
    let failure = failure_identity();
    let evidence = TruthSpikeEvidence::new(2, 91, 104, &failure, &failure)
        .expect("the reset and replay facts are coherent");

    let encoded = evidence
        .to_pretty_json()
        .expect("the bounded evidence document serializes");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("valid JSON");

    assert_eq!(value["schema_version"], 1);
    assert_eq!(
        value["scenario"],
        "commit_then_close_changed_idempotency_key"
    );
    assert_eq!(value["provider_object_count"], 2);
    assert_eq!(value["database_reset"]["before_oid"], 91);
    assert_eq!(value["database_reset"]["after_oid"], 104);
    assert_eq!(
        value["failure_identity"]["invariant_id"],
        "provider-object-unique"
    );
    assert_eq!(value["fresh_replay_same_identity"], true);
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("tiv-local-only-password"));
}

#[test]
fn evidence_rejects_an_unreset_database_or_a_different_replay_failure() {
    let expected = failure_identity();
    let other = FailureIdentity::new(
        InvariantId::new("balanced-ledger").expect("valid invariant"),
        CheckpointId::new("checkout-quiescent").expect("valid checkpoint"),
    );

    assert_eq!(
        TruthSpikeEvidence::new(2, 91, 91, &expected, &expected),
        Err(EvidenceError::DatabaseWasNotRecreated)
    );
    assert_eq!(
        TruthSpikeEvidence::new(2, 91, 104, &expected, &other),
        Err(EvidenceError::ReplayFailureChanged)
    );
}

fn failure_identity() -> FailureIdentity {
    FailureIdentity::new(
        InvariantId::new("provider-object-unique").expect("valid invariant"),
        CheckpointId::new("checkout-quiescent").expect("valid checkpoint"),
    )
}
