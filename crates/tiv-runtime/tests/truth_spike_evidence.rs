use tiv_core::result::{AttemptResult, CheckpointId, FailureIdentity, InvariantId};
use tiv_runtime::evidence::{EvidenceError, TruthSpikeEvidence};

#[test]
fn evidence_records_three_fresh_attempts_and_a_stable_classification_without_secrets() {
    let failure = failure_identity();
    let attempts = [
        AttemptResult::Violation(failure.clone()),
        AttemptResult::Violation(failure.clone()),
        AttemptResult::Violation(failure.clone()),
    ];
    let evidence = TruthSpikeEvidence::new([2, 2, 2], [91, 104, 117], &failure, &attempts)
        .expect("the reset and reproduction facts are coherent");

    let encoded = evidence
        .to_pretty_json()
        .expect("the bounded evidence document serializes");
    let value: serde_json::Value = serde_json::from_str(&encoded).expect("valid JSON");

    assert_eq!(value["schema_version"], 2);
    assert_eq!(
        value["scenario"],
        "commit_then_close_changed_idempotency_key"
    );
    assert_eq!(
        value["provider_object_counts"],
        serde_json::json!([2, 2, 2])
    );
    assert_eq!(value["database_resets"][0]["before_oid"], 91);
    assert_eq!(value["database_resets"][0]["after_oid"], 104);
    assert_eq!(value["database_resets"][1]["before_oid"], 104);
    assert_eq!(value["database_resets"][1]["after_oid"], 117);
    assert_eq!(
        value["failure_identity"]["invariant_id"],
        "provider-object-unique"
    );
    assert_eq!(value["reproduction"]["attempt_count"], 3);
    assert_eq!(value["reproduction"]["matching_failure_count"], 3);
    assert_eq!(value["reproduction"]["classification"], "stable");
    assert!(value.get("fresh_replay_same_identity").is_none());
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("tiv-local-only-password"));
}

#[test]
fn evidence_classifies_two_matching_failures_as_reproducible() {
    let expected = failure_identity();
    let other = FailureIdentity::new(
        InvariantId::new("balanced-ledger").expect("valid invariant"),
        CheckpointId::new("checkout-quiescent").expect("valid checkpoint"),
    );
    let attempts = [
        AttemptResult::Violation(expected.clone()),
        AttemptResult::Violation(other),
        AttemptResult::Violation(expected.clone()),
    ];

    let evidence = TruthSpikeEvidence::new([2, 2, 2], [91, 104, 117], &expected, &attempts)
        .expect("a non-matching attempt is classified rather than rejected");
    let value: serde_json::Value = serde_json::from_str(
        &evidence
            .to_pretty_json()
            .expect("the bounded evidence serializes"),
    )
    .expect("valid JSON");

    assert_eq!(value["reproduction"]["matching_failure_count"], 2);
    assert_eq!(value["reproduction"]["classification"], "reproducible");
}

#[test]
fn evidence_classifies_one_matching_failure_as_inconclusive() {
    let expected = failure_identity();
    let attempts = [
        AttemptResult::Violation(expected.clone()),
        AttemptResult::Held,
        AttemptResult::Inconclusive,
    ];

    let evidence = TruthSpikeEvidence::new([2, 2, 2], [91, 104, 117], &expected, &attempts)
        .expect("mixed attempts still produce bounded evidence");
    let value: serde_json::Value = serde_json::from_str(
        &evidence
            .to_pretty_json()
            .expect("the bounded evidence serializes"),
    )
    .expect("valid JSON");

    assert_eq!(value["reproduction"]["matching_failure_count"], 1);
    assert_eq!(value["reproduction"]["classification"], "inconclusive");
}

#[test]
fn evidence_rejects_incoherent_provider_or_database_facts() {
    let expected = failure_identity();
    let attempts = [
        AttemptResult::Violation(expected.clone()),
        AttemptResult::Violation(expected.clone()),
        AttemptResult::Violation(expected.clone()),
    ];

    assert_eq!(
        TruthSpikeEvidence::new([2, 1, 2], [91, 104, 117], &expected, &attempts),
        Err(EvidenceError::TooFewProviderObjects)
    );
    assert_eq!(
        TruthSpikeEvidence::new([2, 2, 2], [91, 104, 104], &expected, &attempts),
        Err(EvidenceError::DatabaseWasNotRecreated)
    );
    assert_eq!(
        TruthSpikeEvidence::new([2, 2, 2], [91, 0, 117], &expected, &attempts),
        Err(EvidenceError::DatabaseWasNotRecreated)
    );
}

fn failure_identity() -> FailureIdentity {
    FailureIdentity::new(
        InvariantId::new("provider-object-unique").expect("valid invariant"),
        CheckpointId::new("checkout-quiescent").expect("valid checkpoint"),
    )
}
