use tiv_core::result::{
    AttemptResult, CheckpointId, FailureIdentity, InvalidIdentifier, InvariantId,
    ReproductionClass, classify_reproduction,
};

#[test]
fn failure_identity_parts_reject_empty_values() {
    assert_eq!(InvariantId::new(""), Err(InvalidIdentifier::Empty));
    assert_eq!(CheckpointId::new(""), Err(InvalidIdentifier::Empty));
}

#[test]
fn failure_identity_parts_reject_whitespace_only_values() {
    assert_eq!(InvariantId::new("  \n"), Err(InvalidIdentifier::Empty));
    assert_eq!(CheckpointId::new("\t"), Err(InvalidIdentifier::Empty));
}

#[test]
fn three_matching_fresh_baseline_failures_are_stable() {
    let identity = failure_identity("provider-object-unique", "after-quiescence");
    let attempts = [
        AttemptResult::Violation(identity.clone()),
        AttemptResult::Violation(identity.clone()),
        AttemptResult::Violation(identity.clone()),
    ];

    let classification = classify_reproduction(&identity, &attempts);

    assert_eq!(classification, ReproductionClass::Stable);
}

#[test]
fn two_matching_fresh_baseline_failures_are_reproducible() {
    let identity = failure_identity("provider-object-unique", "after-quiescence");
    let other = failure_identity("terminal-state-monotonic", "after-quiescence");
    let attempts = [
        AttemptResult::Violation(identity.clone()),
        AttemptResult::Violation(other),
        AttemptResult::Violation(identity.clone()),
    ];

    let classification = classify_reproduction(&identity, &attempts);

    assert_eq!(classification, ReproductionClass::Reproducible);
}

#[test]
fn one_matching_failure_is_inconclusive() {
    let identity = failure_identity("provider-object-unique", "after-quiescence");
    let attempts = [
        AttemptResult::Violation(identity.clone()),
        AttemptResult::Held,
        AttemptResult::Inconclusive,
    ];

    let classification = classify_reproduction(&identity, &attempts);

    assert_eq!(classification, ReproductionClass::Inconclusive);
}

fn failure_identity(invariant: &str, checkpoint: &str) -> FailureIdentity {
    FailureIdentity::new(
        InvariantId::new(invariant).expect("test invariant IDs are valid"),
        CheckpointId::new(checkpoint).expect("test checkpoint IDs are valid"),
    )
}
