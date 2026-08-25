use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, ManagedFixture, PaymentIntentFixture,
    http::{serve_http1_connection, serve_managed_http1_connection},
};
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
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.expect("a client connects");
            serve_http1_connection(stream, Arc::clone(&server_fixture), FaultOutcome::Normal)
                .await
                .expect("the Stripe-shaped connection is served");
        }
    });
    let client = reqwest::Client::new();
    let collection_url = format!("http://{address}/v1/payment_intents");

    let created: serde_json::Value = client
        .post(&collection_url)
        .header("Idempotency-Key", "checkout-order-42")
        .header("Connection", "close")
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
        .header("Connection", "close")
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
        .header("Connection", "close")
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
async fn committed_confirmation_close_is_a_real_transport_failure_with_one_event() {
    let mut model = PaymentIntentFixture::new(Seed::new(42));
    let created = model
        .create(
            IdempotencyKey::new("checkout-order-42").unwrap(),
            CreatePaymentIntent::new(2_500, "usd").unwrap(),
            FaultOutcome::Normal,
        )
        .expect("the provider object exists before confirmation");
    let payment_intent_id = created.id().to_owned();
    let fixture = Arc::new(Mutex::new(model));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        for outcome in [FaultOutcome::CommitThenClose, FaultOutcome::Normal] {
            let (stream, _) = listener.accept().await.unwrap();
            let _result =
                serve_http1_connection(stream, Arc::clone(&server_fixture), outcome).await;
        }
    });
    let url = format!("http://{address}/v1/payment_intents/{payment_intent_id}/confirm");
    let client = reqwest::Client::new();

    let first = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await;
    assert!(
        first.is_err(),
        "the committed confirmation response is lost"
    );
    assert_eq!(fixture.lock().await.events().len(), 1);

    let retry = client
        .post(url)
        .header("Connection", "close")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .expect("the confirmation retry receives provider state");
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        retry.json::<serde_json::Value>().await.unwrap()["status"],
        "succeeded"
    );

    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.lock().await.events().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_confirmation_delay_waits_for_the_exact_managed_gate() {
    let mut model = ManagedFixture::new(Seed::new(42));
    model
        .reset(
            1,
            Seed::new(42),
            vec![FaultOutcome::Normal, FaultOutcome::CommitThenDelay],
        )
        .unwrap();
    model
        .create_data_plane(
            IdempotencyKey::new("checkout-order-42").unwrap(),
            CreatePaymentIntent::new(2_500, "usd").unwrap(),
        )
        .unwrap();
    let payment_intent_id = model.snapshot().payment_intents()[0].id().to_owned();
    let fixture = Arc::new(Mutex::new(model));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_managed_http1_connection(stream, server_fixture)
            .await
            .unwrap();
    });
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!(
                "http://{address}/v1/payment_intents/{payment_intent_id}/confirm"
            ))
            .header("Connection", "close")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("")
            .send()
            .await
    });

    let gate_id = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = fixture.lock().await.snapshot();
            if let Some(gate) = snapshot.held_gates().first() {
                assert_eq!(snapshot.payment_intents()[0].status(), "succeeded");
                break gate.gate_id();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the committed confirmation reaches a held boundary");
    assert!(!request.is_finished());
    fixture
        .lock()
        .await
        .release_gate(2, gate_id)
        .expect("the exact next control command releases the response");

    let response = timeout(Duration::from_secs(2), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
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
async fn operation_search_recovers_an_object_hidden_by_post_execution_500() {
    let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.expect("a client connects");
            let _result = serve_http1_connection(
                stream,
                Arc::clone(&server_fixture),
                FaultOutcome::PostExecute500,
            )
            .await;
        }
    });
    let client = reqwest::Client::new();
    let collection_url = format!("http://{address}/v1/payment_intents");
    let first = client
        .post(&collection_url)
        .header("Idempotency-Key", "checkout-order-42")
        .header("Connection", "close")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("amount=2500&currency=usd&metadata%5Boperation_id%5D=op_1")
        .send()
        .await
        .expect("the injected failure is an HTTP response");
    assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let recovered: serde_json::Value = client
        .get(format!("{collection_url}/search?operation_id=op_1"))
        .header("Connection", "close")
        .send()
        .await
        .expect("the operation lookup receives a response")
        .error_for_status()
        .expect("the operation lookup succeeds")
        .json()
        .await
        .expect("the operation lookup is JSON");
    let missing: serde_json::Value = client
        .get(format!("{collection_url}/search?operation_id=op_missing"))
        .header("Connection", "close")
        .send()
        .await
        .expect("the empty operation lookup receives a response")
        .error_for_status()
        .expect("the empty operation lookup succeeds")
        .json()
        .await
        .expect("the empty operation lookup is JSON");

    assert_eq!(recovered["object"], "list");
    assert_eq!(recovered["has_more"], false);
    assert_eq!(recovered["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(recovered["data"][0]["metadata"]["operation_id"], "op_1");
    assert_eq!(missing["data"], serde_json::json!([]));
    assert_eq!(fixture.lock().await.payment_intent_count(), 1);
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the search connection stops")
        .expect("the server task does not panic");
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
