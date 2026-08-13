use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    FaultOutcome, ManagedFixture, PaymentIntentFixture,
    http::{serve_http1_connection, serve_managed_http1_connection},
};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_then_close_is_a_real_transport_failure_with_a_cached_retry() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);

    let server = tokio::spawn(async move {
        for outcome in [FaultOutcome::CommitThenClose, FaultOutcome::Normal] {
            let (stream, _) = listener.accept().await.expect("a client connects");
            let _result =
                serve_http1_connection(stream, Arc::clone(&server_fixture), outcome).await;
        }
    });

    let url = format!("http://{address}/v1/payment_intents");
    let form = [("amount", "2500"), ("currency", "usd")];

    let client = reqwest::Client::new();
    let first = client
        .post(&url)
        .header("Idempotency-Key", "checkout-order-42")
        .form(&form)
        .send()
        .await;
    assert!(first.is_err(), "the committed first response must be lost");

    let retry = client
        .post(&url)
        .header("Idempotency-Key", "checkout-order-42")
        .form(&form)
        .send()
        .await
        .expect("the retry receives the cached response");
    assert_eq!(retry.status(), StatusCode::OK);
    let response: serde_json::Value = retry.json().await.expect("the response is JSON");
    assert!(
        response["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("pi_tiv_"))
    );
    drop(client);

    timeout(Duration::from_secs(2), server)
        .await
        .expect("the two-connection server stops")
        .expect("the server task does not panic");
    assert_eq!(fixture.lock().await.payment_intent_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_post_execution_500_replays_the_exact_cached_http_response() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);

    let server = tokio::spawn(async move {
        for outcome in [FaultOutcome::PostExecute500, FaultOutcome::Normal] {
            let (stream, _) = listener.accept().await.expect("a client connects");
            let _result =
                serve_http1_connection(stream, Arc::clone(&server_fixture), outcome).await;
        }
    });

    let first_client = reqwest::Client::new();
    let url = format!("http://{address}/v1/payment_intents");
    let form = [("amount", "2500"), ("currency", "usd")];

    let first = first_client
        .post(&url)
        .header("Idempotency-Key", "checkout-order-42")
        .form(&form)
        .send()
        .await
        .expect("the injected failure is an HTTP response");
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let first_body = first.bytes().await.expect("the first body is readable");
    drop(first_client);

    let retry_client = reqwest::Client::new();
    let retry = retry_client
        .post(&url)
        .header("Idempotency-Key", "checkout-order-42")
        .form(&form)
        .send()
        .await
        .expect("the retry receives the cached failure");
    assert_eq!(retry.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let retry_body = retry.bytes().await.expect("the retry body is readable");

    assert_eq!(retry_body, first_body);
    drop(retry_client);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the two-connection server stops")
        .expect("the server task does not panic");
    assert_eq!(fixture.lock().await.payment_intent_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_form_fields_are_rejected_before_idempotent_execution() {
    assert_rejected_body("amount=2500&amount=3000&currency=usd").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_form_fields_are_rejected_before_idempotent_execution() {
    assert_rejected_body("amount=2500&currency=usd&description=ignored").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_metadata_is_accepted_through_the_exact_supported_form_field() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a client connects");
        let _result = serve_http1_connection(stream, server_fixture, FaultOutcome::Normal).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/payment_intents"))
        .header("Idempotency-Key", "checkout-order-42")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("amount=2500&currency=usd&metadata%5Boperation_id%5D=op_1")
        .send()
        .await
        .expect("the create returns an HTTP response");
    assert_eq!(response.status(), StatusCode::OK);
    let response: serde_json::Value = response.json().await.expect("the response is JSON");
    assert_eq!(response["metadata"]["operation_id"], "op_1");

    timeout(Duration::from_secs(2), server)
        .await
        .expect("the one-connection server stops")
        .expect("the server task does not panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saleor_checkout_create_shape_returns_client_secret_and_metadata() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a client connects");
        let _result = serve_http1_connection(stream, server_fixture, FaultOutcome::Normal).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/payment_intents"))
        .header("Idempotency-Key", "stripe-python-generated-key-1")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(
            "amount=2500&currency=usd&capture_method=automatic\
             &metadata%5Bchannel%5D=default-channel\
             &metadata%5Bpayment_id%5D=UGF5bWVudDox\
             &receipt_email=buyer%40example.test",
        )
        .send()
        .await
        .expect("the create returns an HTTP response");
    assert_eq!(response.status(), StatusCode::OK);
    let response: serde_json::Value = response.json().await.expect("the response is JSON");

    assert!(
        response["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("pi_tiv_"))
    );
    assert!(
        response["client_secret"]
            .as_str()
            .is_some_and(|secret| secret.starts_with("pi_tiv_"))
    );
    assert_eq!(response["metadata"]["channel"], "default-channel");
    assert_eq!(response["metadata"]["payment_id"], "UGF5bWVudDox");

    timeout(Duration::from_secs(2), server)
        .await
        .expect("the one-connection server stops")
        .expect("the server task does not panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saleor_checkout_create_accepts_uppercase_currency_and_normalizes_provider_state() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a client connects");
        let _result = serve_http1_connection(stream, server_fixture, FaultOutcome::Normal).await;
    });

    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/payment_intents"))
        .header("Idempotency-Key", "stripe-python-generated-key-1")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(
            "amount=2500&currency=USD&capture_method=automatic\
             &metadata%5Bchannel%5D=main\
             &metadata%5Bpayment_id%5D=UGF5bWVudDox\
             &receipt_email=buyer%40example.test",
        )
        .send()
        .await
        .expect("the create returns an HTTP response");
    assert_eq!(response.status(), StatusCode::OK);
    let response: serde_json::Value = response.json().await.expect("the response is JSON");
    assert_eq!(response["currency"], "usd");

    timeout(Duration::from_secs(2), server)
        .await
        .expect("the one-connection server stops")
        .expect("the server task does not panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saleor_metadata_is_retained_in_provider_state_after_changed_key_retry() {
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
            let (stream, _) = listener.accept().await.expect("a client connects");
            let _result = serve_managed_http1_connection(stream, Arc::clone(&server_fixture)).await;
        }
    });

    let client = reqwest::Client::new();
    let body = "amount=2500&currency=usd&capture_method=automatic\
                &metadata%5Bchannel%5D=default-channel\
                &metadata%5Bpayment_id%5D=UGF5bWVudDox\
                &receipt_email=buyer%40example.test";
    let url = format!("http://{address}/v1/payment_intents");
    let first = client
        .post(&url)
        .header("Idempotency-Key", "stripe-python-generated-key-1")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await;
    assert!(first.is_err(), "the committed first response must be lost");

    let retry = client
        .post(&url)
        .header("Idempotency-Key", "stripe-python-generated-key-2")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("the retry receives the second provider response");
    assert_eq!(retry.status(), StatusCode::OK);
    drop(retry);
    drop(client);

    timeout(Duration::from_secs(2), server)
        .await
        .expect("the two-connection server stops")
        .expect("the server task does not panic");
    let snapshot =
        serde_json::to_value(fixture.lock().await.snapshot()).expect("the snapshot is JSON");
    let provider_objects = snapshot["payment_intents"]
        .as_array()
        .expect("the snapshot contains provider objects");
    assert_eq!(provider_objects.len(), 2);
    assert_eq!(
        provider_objects[0]["metadata"]["payment_id"],
        "UGF5bWVudDox"
    );
    assert_eq!(
        provider_objects[1]["metadata"]["payment_id"],
        "UGF5bWVudDox"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_or_invalid_operation_metadata_is_rejected_before_execution() {
    assert_rejected_body(
        "amount=2500&currency=usd&metadata%5Boperation_id%5D=op_1&metadata%5Boperation_id%5D=op_2",
    )
    .await;
    assert_rejected_body("amount=2500&currency=usd&metadata%5Boperation_id%5D=%20").await;
}

async fn assert_rejected_body(body: &'static str) {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a client connects");
        let _result = serve_http1_connection(stream, server_fixture, FaultOutcome::Normal).await;
    });

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{address}/v1/payment_intents"))
        .header("Idempotency-Key", "checkout-order-42")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .expect("the validation failure is an HTTP response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    drop(response);
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the one-connection server stops")
        .expect("the server task does not panic");
    assert_eq!(fixture.lock().await.payment_intent_count(), 0);
}
