use tiv_core::{
    decision::Seed,
    plan::{ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec},
};
use tiv_stripe_pi::{CreatePaymentIntent, FaultOutcome, IdempotencyKey, PaymentIntentFixture};

#[test]
fn planned_event_cardinality_matches_the_real_fixture_projection() {
    for seed in 0..128_u64 {
        let plan = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
            Seed::new(seed),
            ActionBudget::new(40).unwrap(),
        ))
        .expect("the v1 plan is feasible");
        let committed_objects = plan
            .actions()
            .iter()
            .map(|action| committed_business_create(action.kind()))
            .sum::<usize>();
        let planned_events = plan
            .actions()
            .iter()
            .filter(|action| matches!(action.kind(), PlanActionKind::GenerateProviderEvent))
            .count();

        let mut fixture = PaymentIntentFixture::new(Seed::new(seed));
        let mut ids = Vec::new();
        for index in 0..committed_objects {
            let created = fixture
                .create(
                    IdempotencyKey::new(format!("case-{seed}-attempt-{index}")).unwrap(),
                    CreatePaymentIntent::new(2_500, "usd").unwrap(),
                    FaultOutcome::Normal,
                )
                .expect("each abstract commit creates one fixture object");
            ids.push(created.id().to_owned());
        }
        for id in ids {
            fixture
                .confirm(&id)
                .expect("each fixture object enters terminal success once");
        }

        assert_eq!(fixture.events().len(), planned_events);
        if plan
            .actions()
            .iter()
            .any(|action| matches!(action.kind(), PlanActionKind::ReorderWebhooks))
        {
            assert!(fixture.events().len() >= 2);
        }
    }
}

fn committed_business_create(kind: &PlanActionKind) -> usize {
    match kind {
        PlanActionKind::DriveCheckout { provider_script }
        | PlanActionKind::RetryBusinessRequest { provider_script } => {
            usize::from(provider_script.committed_count())
        }
        _ => 0,
    }
}
