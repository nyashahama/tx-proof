use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use tiv_core::decision::Seed;
use tiv_stripe_pi::{FaultOutcome, PaymentIntentFixture, http::serve_http1_connection};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stripe_shaped_client_can_create_confirm_and_retrieve() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a client connects");
        serve_http1_connection(stream, server_fixture, FaultOutcome::Normal)
            .await
            .expect("the Stripe-shaped connection is served");
    });
    let client = reqwest::Client::new();
    let collection_url = format!("http://{address}/v1/payment_intents");

    let created: serde_json::Value = client
        .post(&collection_url)
        .header("Idempotency-Key", "checkout-order-42")
        .form(&[("amount", "2500"), ("currency", "usd")])
        .send()
        .await
        .expect("the create receives a response")
        .error_for_status()
        .expect("the create succeeds")
        .json()
        .await
        .expect("the create response is JSON");
    let payment_intent_id = created["id"]
        .as_str()
        .expect("the response contains a provider ID");
    let instance_url = format!("{collection_url}/{payment_intent_id}");
    let confirmed: serde_json::Value = client
        .post(format!("{instance_url}/confirm"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .expect("the confirm receives a response")
        .error_for_status()
        .expect("the confirm succeeds")
        .json()
        .await
        .expect("the confirm response is JSON");
    let retrieved: serde_json::Value = client
        .get(instance_url)
        .send()
        .await
        .expect("the retrieve receives a response")
        .error_for_status()
        .expect("the retrieve succeeds")
        .json()
        .await
        .expect("the retrieve response is JSON");

    assert_eq!(confirmed["id"], payment_intent_id);
    assert_eq!(confirmed["status"], "succeeded");
    assert_eq!(retrieved, confirmed);
    assert_eq!(fixture.lock().await.events().len(), 1);
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the connection stops")
        .expect("the server does not panic");
}

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
