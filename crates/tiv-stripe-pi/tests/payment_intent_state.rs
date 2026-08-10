use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    CreatePaymentIntent, EventKind, FaultOutcome, IdempotencyKey, PaymentIntentFixture,
    PaymentIntentStatus,
};

#[test]
fn create_confirm_and_get_follow_the_supported_state_path() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let created = fixture
        .create(
            IdempotencyKey::new("checkout-order-42").expect("the test key is valid"),
            CreatePaymentIntent::new(2_500, "usd").expect("the create request is valid"),
            FaultOutcome::Normal,
        )
        .expect("the create executes");

    assert_eq!(created.status(), PaymentIntentStatus::RequiresConfirmation);

    let confirmed = fixture
        .confirm(created.id())
        .expect("the created PaymentIntent can be confirmed");
    let retrieved = fixture
        .get(created.id())
        .expect("the confirmed PaymentIntent can be retrieved");

    assert_eq!(confirmed.status(), PaymentIntentStatus::Succeeded);
    assert_eq!(retrieved, confirmed);
}

#[test]
fn entering_succeeded_generates_one_immutable_provider_event() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let created = fixture
        .create(
            IdempotencyKey::new("checkout-order-42").expect("the test key is valid"),
            CreatePaymentIntent::new(2_500, "usd").expect("the create request is valid"),
            FaultOutcome::Normal,
        )
        .expect("the create executes");

    fixture
        .confirm(created.id())
        .expect("the created PaymentIntent can be confirmed");
    fixture
        .confirm(created.id())
        .expect("confirming terminal success is idempotent");

    let events = fixture.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), EventKind::PaymentIntentSucceeded);
    assert_eq!(events[0].payment_intent_id(), created.id());
}
