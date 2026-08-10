use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    CreatePaymentIntent, DataPlaneDisposition, FaultOutcome, FixtureError, IdempotencyKey,
    InvalidCreateRequest, PaymentIntentFixture,
};

#[test]
fn retrying_the_same_executed_create_returns_the_first_result() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    let request = valid_create(2_500, "usd");

    let first = fixture
        .create(key.clone(), request.clone(), FaultOutcome::Normal)
        .expect("the first create executes");
    let retry = fixture
        .create(key, request, FaultOutcome::Normal)
        .expect("the matching retry returns the cached result");

    assert_eq!(retry, first);
    assert_eq!(fixture.payment_intent_count(), 1);
}

#[test]
fn reusing_a_key_with_changed_parameters_is_rejected() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    fixture
        .create(
            key.clone(),
            valid_create(2_500, "usd"),
            FaultOutcome::Normal,
        )
        .expect("the first create executes");

    let error = fixture
        .create(key, valid_create(3_000, "usd"), FaultOutcome::Normal)
        .expect_err("changed parameters cannot reuse an idempotency key");

    assert_eq!(error, FixtureError::IdempotencyConflict);
    assert_eq!(fixture.payment_intent_count(), 1);
}

#[test]
fn a_pre_execution_rejection_is_not_cached() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    let request = valid_create(2_500, "usd");

    let first_error = fixture
        .create(key.clone(), request.clone(), FaultOutcome::PreExecute429)
        .expect_err("the injected request is rejected before execution");
    let retry = fixture.create(key, request, FaultOutcome::Normal);

    assert_eq!(first_error, FixtureError::RateLimited);
    assert!(retry.is_ok());
    assert_eq!(fixture.payment_intent_count(), 1);
}

#[test]
fn a_post_execution_500_is_cached_with_the_created_object() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    let request = valid_create(2_500, "usd");

    let first_error = fixture
        .create(key.clone(), request.clone(), FaultOutcome::PostExecute500)
        .expect_err("the provider executes before returning the injected 500");
    let retry_error = fixture
        .create(key, request, FaultOutcome::Normal)
        .expect_err("the matching retry receives the first cached 500");

    assert_eq!(first_error, FixtureError::ServerError);
    assert_eq!(retry_error, first_error);
    assert_eq!(fixture.payment_intent_count(), 1);
}

#[test]
fn a_post_execution_500_caches_the_exact_wire_status_and_body() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    let request = valid_create(2_500, "usd");

    let first = fixture
        .create_data_plane(key.clone(), request.clone(), FaultOutcome::PostExecute500)
        .expect("the injected provider response is representable");
    let retry = fixture
        .create_data_plane(key, request, FaultOutcome::Normal)
        .expect("the matching retry returns the cached provider response");

    assert_eq!(retry, first);
    let DataPlaneDisposition::Response(response) = first else {
        panic!("a post-execution 500 returns a response");
    };
    assert_eq!(response.status_code(), 500);
    assert_eq!(response.content_type(), "application/json");
    assert!(!response.raw_body().is_empty());
    assert_eq!(fixture.payment_intent_count(), 1);
}

#[test]
fn commit_then_close_caches_the_success_that_the_retry_observes() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    let request = valid_create(2_500, "usd");

    let first_error = fixture
        .create(key.clone(), request.clone(), FaultOutcome::CommitThenClose)
        .expect_err("the first connection closes after the provider commits");
    let retry = fixture
        .create(key, request, FaultOutcome::Normal)
        .expect("the matching retry receives the cached success");

    assert_eq!(first_error, FixtureError::ConnectionClosed);
    assert_eq!(fixture.payment_intent_count(), 1);
    assert!(retry.id().starts_with("pi_tiv_"));
}

#[test]
fn retrying_with_a_different_key_creates_a_second_provider_object() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let request = valid_create(2_500, "usd");

    fixture
        .create(
            IdempotencyKey::new("checkout-order-42-attempt-1").expect("valid key"),
            request.clone(),
            FaultOutcome::CommitThenClose,
        )
        .expect_err("the first committed response is lost");
    fixture
        .create(
            IdempotencyKey::new("checkout-order-42-attempt-2").expect("valid key"),
            request,
            FaultOutcome::Normal,
        )
        .expect("the changed key starts a second execution");

    let provider_objects = fixture.payment_intents();
    assert_eq!(provider_objects.len(), 2);
    assert_ne!(provider_objects[0].id(), provider_objects[1].id());
}

#[test]
fn fixture_ids_are_reproducible_for_a_seed_and_distinct_across_seeds() {
    let first = first_payment_intent(42);
    let same_seed = first_payment_intent(42);
    let other_seed = first_payment_intent(43);

    assert_eq!(same_seed.id(), first.id());
    assert_ne!(other_seed.id(), first.id());
}

#[test]
fn create_requests_reject_non_positive_minor_amounts() {
    assert_eq!(
        CreatePaymentIntent::new(0, "usd"),
        Err(InvalidCreateRequest::NonPositiveAmount)
    );
}

#[test]
fn create_requests_require_a_three_letter_lowercase_currency() {
    for currency in ["", "us", "usdx", "u$d", "USD"] {
        assert_eq!(
            CreatePaymentIntent::new(2_500, currency),
            Err(InvalidCreateRequest::InvalidCurrency),
            "currency {currency:?} must be rejected"
        );
    }
}

#[test]
fn idempotency_keys_reject_blank_or_overlong_values() {
    assert!(IdempotencyKey::new(" \n").is_err());
    assert!(IdempotencyKey::new("x".repeat(256)).is_err());
    assert!(IdempotencyKey::new("x".repeat(255)).is_ok());
}

#[test]
fn a_pre_execution_500_is_not_cached() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let key = IdempotencyKey::new("checkout-order-42").expect("the test key is valid");
    let request = valid_create(2_500, "usd");

    let first_error = fixture
        .create(key.clone(), request.clone(), FaultOutcome::PreExecute500)
        .expect_err("the injected 500 occurs before execution");
    let retry = fixture.create(key, request, FaultOutcome::Normal);

    assert_eq!(first_error, FixtureError::ServerError);
    assert!(retry.is_ok());
    assert_eq!(fixture.payment_intent_count(), 1);
}

fn first_payment_intent(seed: u64) -> tiv_stripe_pi::PaymentIntent {
    let mut fixture = PaymentIntentFixture::new(Seed::new(seed));
    fixture
        .create(
            IdempotencyKey::new("checkout-order-42").expect("the test key is valid"),
            valid_create(2_500, "usd"),
            FaultOutcome::Normal,
        )
        .expect("the create executes")
}

fn valid_create(amount_minor: i64, currency: &str) -> CreatePaymentIntent {
    CreatePaymentIntent::new(amount_minor, currency).expect("test create parameters are valid")
}
