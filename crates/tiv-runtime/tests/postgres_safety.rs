use tiv_runtime::postgres::safety::{
    ComposeProjectId, DatabaseEndpoint, DatabaseIdentity, DatabaseMarker, DatabaseName,
    DatabaseTarget, IdentityField, MarkerKind, SafetyError, Unverified,
};
use uuid::Uuid;

#[test]
fn only_generated_baseline_and_case_database_names_are_accepted() {
    assert!(DatabaseName::parse("tiv_base_01234567").is_ok());
    assert!(DatabaseName::parse("tiv_case_0123456789abcdef").is_ok());

    for invalid in [
        "postgres",
        "tiv_case_",
        "tiv_case_UPPERCASE",
        "tiv_case_has-dash",
        "tiv_case_../postgres",
        "tiv_case_0123456",
    ] {
        assert!(
            DatabaseName::parse(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn a_fresh_exact_identity_match_issues_a_single_use_mutation_permit() {
    let expected = case_identity(91, "11111111-1111-4111-8111-111111111111");
    let observed = expected.clone();
    let target = DatabaseTarget::<Unverified>::new(expected);

    let (verified, _permit) = target
        .verify(&observed)
        .expect("every identity field and the case marker match");

    assert_eq!(verified.identity(), &observed);
}

#[test]
fn an_oid_change_rejects_mutation_before_a_permit_exists() {
    let expected = case_identity(91, "11111111-1111-4111-8111-111111111111");
    let observed = case_identity(92, "11111111-1111-4111-8111-111111111111");

    let error = DatabaseTarget::<Unverified>::new(expected)
        .verify(&observed)
        .expect_err("a recreated or substituted database must not be mutated");

    assert_eq!(
        error,
        SafetyError::IdentityMismatch(IdentityField::DatabaseOid)
    );
}

#[test]
fn a_marker_change_rejects_mutation_even_when_catalog_identity_matches() {
    let expected = case_identity(91, "11111111-1111-4111-8111-111111111111");
    let observed = case_identity(91, "22222222-2222-4222-8222-222222222222");

    let error = DatabaseTarget::<Unverified>::new(expected)
        .verify(&observed)
        .expect_err("the in-database marker is part of the identity tuple");

    assert_eq!(error, SafetyError::IdentityMismatch(IdentityField::Marker));
}

#[test]
fn a_baseline_marker_can_never_authorize_case_mutation() {
    let mut identity = case_identity(91, "11111111-1111-4111-8111-111111111111");
    identity = identity.with_marker(DatabaseMarker::new(
        Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("valid UUID"),
        MarkerKind::Baseline,
        ComposeProjectId::new("tiv-truth-spike-test").expect("valid project ID"),
    ));
    let observed = identity.clone();

    let error = DatabaseTarget::<Unverified>::new(identity)
        .verify(&observed)
        .expect_err("the sealed template is never a mutable case target");

    assert_eq!(error, SafetyError::ExpectedCaseMarker);
}

fn case_identity(database_oid: u32, marker_uuid: &str) -> DatabaseIdentity {
    DatabaseIdentity::new(
        "pg-system-740592390",
        DatabaseEndpoint::loopback(15_432),
        DatabaseName::parse("tiv_case_0123456789abcdef").expect("valid generated name"),
        database_oid,
        10,
        DatabaseMarker::new(
            Uuid::parse_str(marker_uuid).expect("valid UUID"),
            MarkerKind::Case,
            ComposeProjectId::new("tiv-truth-spike-test").expect("valid project ID"),
        ),
        "tiv_app",
    )
    .expect("the identity is complete")
}
