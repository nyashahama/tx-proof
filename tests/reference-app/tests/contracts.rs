use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tiv_core::decision::Seed;
use tiv_reference_app::{
    CheckoutOperation, ReferenceDatabaseName, create_with_changed_retry_key,
    parse_succeeded_webhook, verify_webhook_signature,
};
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, ManagedFixture, OperationId,
    PaymentIntentFixture, http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};

#[test]
fn only_generated_case_database_names_are_accepted() {
    assert!(ReferenceDatabaseName::parse("tiv_case_0123456789abcdef").is_ok());
    for invalid in [
        "postgres",
        "tiv_base_0123456789abcdef",
        "tiv_case_",
        "tiv_case_UPPER",
        "tiv_case_bad-name",
        "tiv_case_bad;drop_database",
        "tiv_case_1",
        "tiv_case_notgenerated",
        "tiv_case_0123456789abcdef_",
        "tiv_case_0123456789abcdef0123456789abcdef0",
    ] {
        assert!(
            ReferenceDatabaseName::parse(invalid).is_err(),
            "{invalid:?} must not be a reference-app target"
        );
    }
}

#[test]
fn webhook_verification_covers_the_exact_raw_bytes_and_rejects_stale_signatures() {
    let secret = b"whsec_test_secret";
    let timestamp = current_unix_timestamp();
    let raw_body = br#"{"id":"evt_1","type":"payment_intent.succeeded"}"#;
    let signature = signature_header(timestamp, raw_body, secret);

    assert!(verify_webhook_signature(raw_body, &signature, secret).is_ok());
    assert!(
        verify_webhook_signature(
            br#"{"id":"evt_1", "type":"payment_intent.succeeded"}"#,
            &signature,
            secret,
        )
        .is_err(),
        "even semantically equivalent byte changes must fail verification"
    );
    let stale_timestamp = timestamp - 301;
    let stale_signature = signature_header(stale_timestamp, raw_body, secret);
    assert!(
        verify_webhook_signature(raw_body, &stale_signature, secret).is_err(),
        "a correctly signed webhook outside the five-minute tolerance must fail"
    );
}

#[test]
fn signed_fixture_webhook_is_decoded_into_the_same_operation_relation() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let created = fixture
        .create(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            CreatePaymentIntent::new(2_500, "usd")
                .expect("the create is valid")
                .with_operation_id(OperationId::new("op_1").expect("the operation ID is valid")),
            FaultOutcome::Normal,
        )
        .expect("the provider object is created");
    fixture
        .confirm(created.id())
        .expect("the provider object confirms");
    let attempt = fixture.events()[0]
        .webhook_attempt(current_unix_timestamp(), b"whsec_test_secret")
        .expect("the event is signed");

    let observed = parse_succeeded_webhook(
        attempt.raw_body(),
        attempt.signature_header(),
        b"whsec_test_secret",
    )
    .expect("the exact signed event is accepted");

    assert_eq!(observed.id(), created.id());
    assert_eq!(observed.operation_id(), "op_1");
    assert_eq!(observed.amount_minor(), 2_500);
    assert_eq!(observed.currency(), "usd");
    assert_eq!(observed.status(), "succeeded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_transport_failure_is_retried_with_a_changed_key() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(42))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(42),
            vec![FaultOutcome::CommitThenClose, FaultOutcome::Normal],
        )
        .expect("the fault plan is installed");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.expect("the app connects");
            let _result = serve_managed_http1_connection(stream, Arc::clone(&server_fixture)).await;
        }
    });
    let operation =
        CheckoutOperation::new("op_1", 2_500, "usd").expect("the checkout operation is valid");

    let observed = create_with_changed_retry_key(
        &reqwest::Client::new(),
        &format!("http://{address}"),
        &operation,
    )
    .await
    .expect("the faulty retry receives the second provider object");

    assert!(observed.id().starts_with("pi_tiv_"));
    assert_eq!(observed.operation_id(), "op_1");
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the two-connection fixture stops")
        .expect("the fixture task does not panic");
    let snapshot = fixture.lock().await.snapshot();
    assert_eq!(snapshot.payment_intents().len(), 2);
    assert_ne!(
        snapshot.payment_intents()[0].id(),
        snapshot.payment_intents()[1].id()
    );
}

fn signature_header(timestamp: i64, body: &[u8], secret: &[u8]) -> String {
    let mut signer = Hmac::<Sha256>::new_from_slice(secret).expect("the test secret is valid");
    signer.update(timestamp.to_string().as_bytes());
    signer.update(b".");
    signer.update(body);
    format!(
        "t={timestamp},v1={}",
        hex::encode(signer.finalize().into_bytes())
    )
}

fn current_unix_timestamp() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_secs(),
    )
    .expect("the current Unix timestamp fits in i64")
}
