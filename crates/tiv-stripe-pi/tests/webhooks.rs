use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    CreatePaymentIntent, DataPlaneDisposition, FaultOutcome, IdempotencyKey, OperationId,
    PaymentIntentFixture, WebhookSignatureError,
};

#[test]
fn webhook_attempts_preserve_raw_event_bytes_and_refresh_the_signature() {
    let event = succeeded_event();
    let secret = b"whsec_test_secret";

    let first = event
        .webhook_attempt(1_700_000_000, secret)
        .expect("the test secret can sign an attempt");
    let retry = event
        .webhook_attempt(1_700_000_001, secret)
        .expect("the test secret can sign a retry");

    assert_eq!(retry.event_id(), first.event_id());
    assert_eq!(retry.raw_body(), first.raw_body());
    assert_ne!(retry.signature_header(), first.signature_header());
    assert_eq!(
        first.signature_header(),
        expected_signature_header(first.timestamp(), first.raw_body(), secret)
    );
}

#[test]
fn webhook_signing_rejects_an_empty_secret() {
    let error = succeeded_event()
        .webhook_attempt(1_700_000_000, b"")
        .expect_err("an empty webhook secret is unsafe");

    assert_eq!(error, WebhookSignatureError::EmptySecret);
}

#[test]
fn operation_metadata_round_trips_through_create_and_webhook_wire_bytes() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let create = CreatePaymentIntent::new(2_500, "usd")
        .expect("the create request is valid")
        .with_operation_id(OperationId::new("op_1").expect("the operation ID is valid"));
    let disposition = fixture
        .create_data_plane(
            IdempotencyKey::new("checkout-order-42").expect("the test key is valid"),
            create,
            FaultOutcome::Normal,
        )
        .expect("the create executes");
    let DataPlaneDisposition::Response(response) = disposition else {
        panic!("the normal create returns an HTTP response");
    };
    let create_json: serde_json::Value =
        serde_json::from_slice(response.raw_body()).expect("the create response is JSON");
    let payment_intent_id = create_json["id"]
        .as_str()
        .expect("the response contains a PaymentIntent ID");

    fixture
        .confirm(payment_intent_id)
        .expect("the PaymentIntent confirms");
    let event_json: serde_json::Value = serde_json::from_slice(
        fixture
            .events()
            .first()
            .expect("confirmation emits one event")
            .webhook_attempt(1_700_000_000, b"whsec_test_secret")
            .expect("the event can be signed")
            .raw_body(),
    )
    .expect("the webhook body is JSON");

    assert_eq!(create_json["metadata"]["operation_id"], "op_1");
    assert_eq!(
        event_json["data"]["object"]["metadata"]["operation_id"],
        "op_1"
    );
}

fn succeeded_event() -> tiv_stripe_pi::ProviderEvent {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let payment_intent = fixture
        .create(
            IdempotencyKey::new("checkout-order-42").expect("the test key is valid"),
            CreatePaymentIntent::new(2_500, "usd").expect("the create request is valid"),
            FaultOutcome::Normal,
        )
        .expect("the create executes");
    fixture
        .confirm(payment_intent.id())
        .expect("the PaymentIntent confirms");
    fixture.events()[0].clone()
}

fn expected_signature_header(timestamp: i64, raw_body: &[u8], secret: &[u8]) -> String {
    let mut signer = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts this test key");
    signer.update(timestamp.to_string().as_bytes());
    signer.update(b".");
    signer.update(raw_body);
    let signature = hex::encode(signer.finalize().into_bytes());
    format!("t={timestamp},v1={signature}")
}
