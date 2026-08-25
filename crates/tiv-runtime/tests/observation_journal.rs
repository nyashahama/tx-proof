use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use tiv_core::trace::ActionId;
use tiv_runtime::journal::{
    JournalContext, JournalError, JournalLimits, Observation, ObservationEvent, ObservationJournal,
    ObservationProducer,
};
use tokio::time::timeout;
use uuid::Uuid;

#[tokio::test]
async fn flushed_observations_receive_one_global_sequence_and_a_hash_link() {
    let path =
        std::env::temp_dir().join(format!("tiv-observation-journal-{}.ndjson", Uuid::new_v4()));
    let journal = ObservationJournal::create(
        &path,
        JournalLimits::new(1, 2).expect("the tiny test limits are valid"),
    )
    .await
    .expect("a new private journal is created");
    let context = JournalContext::new("run_1", "case_1", ActionId::new(7))
        .expect("the journal IDs are valid");

    let held = journal
        .append(Observation::new(
            context.clone(),
            ObservationProducer::Fixture,
            1,
            1_200,
            ObservationEvent::ProviderResponseHeld { gate_id: 1 },
        ))
        .await
        .expect("the held observation is flushed");
    let released = journal
        .append(Observation::new(
            context,
            ObservationProducer::Orchestrator,
            1,
            1_500,
            ObservationEvent::ProviderResponseReleased { gate_id: 1 },
        ))
        .await
        .expect("the released observation is flushed");

    assert_eq!(held.observed_sequence(), 1);
    assert_eq!(released.observed_sequence(), 2);
    assert_eq!(released.previous_hash(), held.record_hash());
    let on_disk = tokio::fs::read_to_string(&path)
        .await
        .expect("acknowledged records are already readable");
    assert_eq!(
        on_disk,
        include_str!("../../../tests/golden/observation-journal-v1.ndjson")
    );

    let summary = timeout(Duration::from_secs(2), journal.finish())
        .await
        .expect("the writer terminates")
        .expect("the journal finalizes");
    assert_eq!(summary.record_count(), 2);
    assert_eq!(summary.last_record_hash(), Some(released.record_hash()));
    tokio::fs::remove_file(path)
        .await
        .expect("the exact test journal is removed");
}

#[tokio::test]
async fn producer_sequence_and_record_limits_reject_without_consuming_global_order() {
    let path =
        std::env::temp_dir().join(format!("tiv-observation-journal-{}.ndjson", Uuid::new_v4()));
    let journal = ObservationJournal::create(
        &path,
        JournalLimits::new(1, 1).expect("the tiny test limits are valid"),
    )
    .await
    .expect("a new private journal is created");
    let context = JournalContext::new("run_1", "case_1", ActionId::new(7))
        .expect("the journal IDs are valid");

    let out_of_order = journal
        .append(Observation::new(
            context.clone(),
            ObservationProducer::Fixture,
            2,
            1_000,
            ObservationEvent::ActionIntent,
        ))
        .await
        .expect_err("a producer must start at its first sequence");
    let accepted = journal
        .append(Observation::new(
            context.clone(),
            ObservationProducer::Fixture,
            1,
            1_100,
            ObservationEvent::ActionIntent,
        ))
        .await
        .expect("the rejected observation consumed no order");
    let over_limit = journal
        .append(Observation::new(
            context,
            ObservationProducer::Fixture,
            2,
            1_200,
            ObservationEvent::ActionOutcome,
        ))
        .await
        .expect_err("the final record bound is enforced");

    assert!(matches!(
        out_of_order,
        JournalError::UnexpectedProducerSequence {
            expected: 1,
            received: 2
        }
    ));
    assert_eq!(accepted.observed_sequence(), 1);
    assert!(matches!(over_limit, JournalError::RecordLimit));
    let summary = journal.finish().await.expect("the journal finalizes");
    assert_eq!(summary.record_count(), 1);
    tokio::fs::remove_file(path)
        .await
        .expect("the exact test journal is removed");
}

#[tokio::test]
async fn journal_paths_and_identifiers_fail_closed() {
    assert!(JournalLimits::new(0, 1).is_err());
    assert!(JournalLimits::new(1, 0).is_err());
    assert!(JournalContext::new("../run", "case_1", ActionId::new(1)).is_err());

    let path =
        std::env::temp_dir().join(format!("tiv-observation-journal-{}.ndjson", Uuid::new_v4()));
    let journal = ObservationJournal::create(
        &path,
        JournalLimits::new(1, 1).expect("the tiny test limits are valid"),
    )
    .await
    .expect("the first journal owns the path");
    let collision = ObservationJournal::create(
        &path,
        JournalLimits::new(1, 1).expect("the tiny test limits are valid"),
    )
    .await;

    assert!(matches!(collision, Err(JournalError::Create { .. })));
    #[cfg(unix)]
    assert_eq!(
        tokio::fs::metadata(&path)
            .await
            .expect("the journal metadata is readable")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    journal.finish().await.expect("the journal finalizes");
    tokio::fs::remove_file(path)
        .await
        .expect("the exact test journal is removed");
}
