use std::collections::BTreeSet;

use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, ProviderOutcome,
        ProviderOutcomeScript,
    },
    shrink::{CandidateGenerator, CandidateLimit, InvalidCandidateLimit, ShrinkCandidate},
    trace::ActionId,
};

fn source(seed: u64) -> tiv_core::plan::PlannedCase {
    CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
        Seed::new(seed),
        ActionBudget::new(40).unwrap(),
    ))
    .expect("the bounded v1 case is feasible")
}

#[test]
fn candidate_limit_is_typed_and_cannot_expand_the_v1_search_bound() {
    assert_eq!(CandidateLimit::new(0), Err(InvalidCandidateLimit::Zero));
    assert_eq!(
        CandidateLimit::new(61),
        Err(InvalidCandidateLimit::AboveV1Maximum)
    );
    assert_eq!(CandidateLimit::default().value(), 60);
}

#[test]
fn candidates_are_deterministic_strictly_simpler_and_self_validating() {
    for seed in 0..16 {
        let source = source(seed);
        let generator = CandidateGenerator::new(&source).expect("a seeded plan is accepted");
        let first = generator
            .candidates(None, CandidateLimit::default())
            .unwrap();
        let second = generator
            .candidates(None, CandidateLimit::default())
            .unwrap();

        assert_eq!(first, second, "candidate order changed for seed {seed}");
        assert!(first.len() <= 60);

        let mut identities = BTreeSet::new();
        for (candidate_index, candidate) in first.into_iter().enumerate() {
            candidate.validate().expect("a yielded candidate validates");
            assert!(candidate.complexity() < generator.source_complexity());
            assert!(identities.insert(candidate.canonical_identity()));

            for (index, action) in candidate.actions().iter().enumerate() {
                let sequence = u32::try_from(index + 1).unwrap();
                assert_eq!(action.id(), ActionId::new(sequence));
                assert_eq!(action.logical_sequence(), sequence);
                assert_eq!(action.decision_number(), index as u64);
                let expected_dependencies = if index == 0 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([ActionId::new(sequence - 1)])
                };
                assert_eq!(action.dependencies(), &expected_dependencies);
            }

            if candidate_index == 0 {
                let encoded = serde_json::to_vec(&candidate).unwrap();
                let decoded: ShrinkCandidate = serde_json::from_slice(&encoded).unwrap();
                assert_eq!(decoded, candidate);
                assert_eq!(decoded.canonical_identity(), candidate.canonical_identity());
            }
        }
    }
}

#[test]
fn candidate_batches_report_real_generation_truncation() {
    let source = (0..512)
        .map(source)
        .find(|plan| {
            let full = CandidateGenerator::new(plan)
                .unwrap()
                .candidates(None, CandidateLimit::default())
                .unwrap();
            (2..usize::from(CandidateLimit::default().value())).contains(&full.len())
        })
        .expect("the deterministic corpus has a naturally finite frontier");
    let generator = CandidateGenerator::new(&source).unwrap();
    let full = generator
        .candidates(None, CandidateLimit::default())
        .unwrap();

    let exact_limit = CandidateLimit::new(u8::try_from(full.len()).unwrap()).unwrap();
    let exact_batch = generator
        .candidate_batch(None, exact_limit)
        .expect("exact-limit generation succeeds");
    assert!(!exact_batch.was_truncated());
    assert_eq!(exact_batch.candidates(), full.as_slice());

    let capped_batch = generator
        .candidate_batch(None, CandidateLimit::new(1).unwrap())
        .expect("capped generation succeeds");
    assert!(capped_batch.was_truncated());
    assert_eq!(capped_batch.candidates().len(), 1);
    assert_eq!(&capped_batch.candidates()[0], &full[0]);
}

#[test]
fn candidate_deserialization_rejects_tampered_or_duplicate_lineage() {
    let source = (0..512)
        .map(source)
        .find(|plan| {
            CandidateGenerator::new(plan)
                .unwrap()
                .candidates(None, CandidateLimit::default())
                .unwrap()
                .len()
                >= 2
        })
        .expect("the deterministic corpus has shrinkable plans");
    let candidate = CandidateGenerator::new(&source)
        .unwrap()
        .candidates(None, CandidateLimit::default())
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    let mut invalid_action = serde_json::to_value(&candidate).unwrap();
    invalid_action["schedule"][0]["kind"] =
        serde_json::json!({ "kind": "check_checkpoint", "checkpoint": "final" });
    assert!(serde_json::from_value::<ShrinkCandidate>(invalid_action).is_err());

    let mut duplicate_lineage = serde_json::to_value(candidate).unwrap();
    duplicate_lineage["schedule"][1]["source_action_id"] =
        duplicate_lineage["schedule"][0]["source_action_id"].clone();
    assert!(serde_json::from_value::<ShrinkCandidate>(duplicate_lineage).is_err());
}

#[test]
fn the_v1_generator_reaches_each_representable_simplification_family() {
    let mut saw_retry_or_recovery_removal = false;
    let mut saw_process_fault_removal = false;
    let mut saw_duplicate_removal = false;
    let mut saw_delay_reduction = false;
    let mut saw_reorder_removal = false;
    let mut saw_drop_to_delivery = false;
    let mut saw_provider_simplification = false;

    for seed in 0..512 {
        let source = source(seed);
        let source_kinds = source
            .actions()
            .iter()
            .map(|action| *action.kind())
            .collect::<Vec<_>>();
        let candidates = CandidateGenerator::new(&source)
            .unwrap()
            .candidates(None, CandidateLimit::default())
            .unwrap();

        for candidate in candidates {
            let kinds = candidate
                .actions()
                .iter()
                .map(|action| *action.kind())
                .collect::<Vec<_>>();
            saw_retry_or_recovery_removal |= source_kinds.iter().any(|kind| {
                matches!(
                    kind,
                    PlanActionKind::RetryBusinessRequest { .. }
                        | PlanActionKind::RetryProviderRequest { .. }
                        | PlanActionKind::RetrievePaymentIntent
                        | PlanActionKind::ReleaseProviderGate
                ) && !kinds.contains(kind)
            });
            saw_process_fault_removal |= source_kinds
                .iter()
                .any(|kind| matches!(kind, PlanActionKind::KillApplication { .. }))
                && !kinds
                    .iter()
                    .any(|kind| matches!(kind, PlanActionKind::KillApplication { .. }));
            saw_duplicate_removal |= source_kinds.contains(&PlanActionKind::DuplicateWebhook)
                && !kinds.contains(&PlanActionKind::DuplicateWebhook);
            saw_reorder_removal |= source_kinds.contains(&PlanActionKind::ReorderWebhooks)
                && !kinds.contains(&PlanActionKind::ReorderWebhooks);
            saw_drop_to_delivery |= source_kinds.contains(&PlanActionKind::DropWebhook)
                && kinds.contains(&PlanActionKind::DeliverWebhook)
                && source_kinds
                    .iter()
                    .filter(|kind| matches!(kind, PlanActionKind::DeliverWebhook))
                    .count()
                    < kinds
                        .iter()
                        .filter(|kind| matches!(kind, PlanActionKind::DeliverWebhook))
                        .count();

            let source_delay = source_kinds.iter().find_map(|kind| match kind {
                PlanActionKind::DelayWebhook { milliseconds } => Some(*milliseconds),
                _ => None,
            });
            let candidate_delay = kinds.iter().find_map(|kind| match kind {
                PlanActionKind::DelayWebhook { milliseconds } => Some(*milliseconds),
                _ => None,
            });
            saw_delay_reduction |= source_delay
                .zip(candidate_delay)
                .is_some_and(|(source, candidate)| candidate < source);

            saw_provider_simplification |=
                source_kinds.iter().zip(&kinds).any(|(source, candidate)| {
                    let source_script = provider_script(source);
                    let candidate_script = provider_script(candidate);
                    source_script.is_some_and(|script| {
                        script != ProviderOutcomeScript::single(ProviderOutcome::Normal)
                            && candidate_script
                                == Some(ProviderOutcomeScript::single(ProviderOutcome::Normal))
                    })
                });
        }

        if saw_retry_or_recovery_removal
            && saw_process_fault_removal
            && saw_duplicate_removal
            && saw_delay_reduction
            && saw_reorder_removal
            && saw_drop_to_delivery
            && saw_provider_simplification
        {
            break;
        }
    }

    assert!(saw_retry_or_recovery_removal);
    assert!(saw_process_fault_removal);
    assert!(saw_duplicate_removal);
    assert!(saw_delay_reduction);
    assert!(saw_reorder_removal);
    assert!(saw_drop_to_delivery);
    assert!(saw_provider_simplification);
}

fn provider_script(kind: &PlanActionKind) -> Option<ProviderOutcomeScript> {
    match kind {
        PlanActionKind::DriveCheckout { provider_script }
        | PlanActionKind::RetryBusinessRequest { provider_script }
        | PlanActionKind::ConfirmPaymentIntent { provider_script }
        | PlanActionKind::RetryProviderRequest { provider_script } => Some(*provider_script),
        _ => None,
    }
}
