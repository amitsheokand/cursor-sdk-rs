//! P3 classification tests against real `cursor_sdk::Error` values.
//!
//! Rpc errors come through the in-process FakeBridge so the test asserts
//! on the exact taxonomy the crate produces (Connect code +
//! `SdkErrorDetails`), not on hand-built structs (`RpcError` is
//! non-exhaustive). Transport/Bridge/Config/Decode errors use dead
//! endpoints and bad binaries.

mod support;

use std::time::Duration;

use cursor_sdk::{AgentOptions, ErrorKind, proto};
use cursor_seat::protocol::Outcome;
use cursor_seat::retry::{classify, classify_run_status, should_retry_in_seat};
use hyper::StatusCode;
use support::*;

fn details(code: proto::SdkErrorCode, request_id: &str) -> proto::SdkErrorDetails {
    proto::SdkErrorDetails {
        request_id: Some(request_id.to_string()),
        sdk_error_code: code as i32,
        message: format!("{code:?}"),
        retry_after: Some(prost_types::Duration {
            seconds: 30,
            nanos: 0,
        }),
        ..Default::default()
    }
}

async fn rpc_error(code: proto::SdkErrorCode) -> cursor_sdk::Error {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/GetAgent",
        Reply::error_with_details(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            details(code, "req_0123456789abcdef0123456789abcdef"),
        ),
    );
    client_for(&bridge).get_agent("a1").await.unwrap_err()
}

#[tokio::test]
async fn taxonomy_maps_to_the_outcome_table() {
    use proto::SdkErrorCode as Code;
    let cases = [
        (Code::Unauthorized, Outcome::Bounced, false, false),
        (Code::PlanRequired, Outcome::Bounced, false, false),
        (Code::InvalidModel, Outcome::Bounced, false, false),
        (Code::RateLimitExceeded, Outcome::Busy, true, false),
        (Code::AgentBusy, Outcome::Busy, true, false),
        (Code::UpstreamError, Outcome::StartupError, true, true),
        (Code::InternalError, Outcome::StartupError, true, true),
        (Code::AgentNotFound, Outcome::Stale, false, false),
        (Code::AgentArchived, Outcome::Stale, false, false),
        (Code::ClientCancelled, Outcome::Stale, false, false),
    ];
    for (code, outcome, retryable, in_seat) in cases {
        let error = rpc_error(code).await;
        let failure = classify(&error);
        assert_eq!(failure.outcome, outcome, "for {code:?}");
        assert_eq!(failure.retryable, retryable, "for {code:?}");
        assert_eq!(should_retry_in_seat(&error), in_seat, "for {code:?}");
        // Full request id survives, untruncated.
        assert_eq!(
            failure.request_id.as_deref(),
            Some("req_0123456789abcdef0123456789abcdef"),
            "for {code:?}"
        );
    }
}

#[tokio::test]
async fn busy_carries_retry_after_ms() {
    let error = rpc_error(proto::SdkErrorCode::RateLimitExceeded).await;
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::Busy);
    assert_eq!(failure.error_kind.as_deref(), Some("RateLimited"));
    assert_eq!(failure.retry_after_ms, Some(30_000));

    let busy = rpc_error(proto::SdkErrorCode::AgentBusy).await;
    let failure = classify(&busy);
    assert_eq!(failure.outcome, Outcome::Busy);
    assert_eq!(failure.error_kind.as_deref(), Some("AgentBusy"));
    assert_eq!(failure.retry_after_ms, Some(30_000));
}

#[test]
fn io_error_is_a_startup_error_without_request_id() {
    let error = cursor_sdk::Error::from(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "token file missing",
    ));
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::StartupError);
    assert_eq!(failure.error_kind, None);
    assert_eq!(failure.request_id, None);
    assert!(!failure.retryable);
    assert!(!should_retry_in_seat(&error));
}

#[tokio::test]
async fn unknown_keeps_its_code_and_fails_without_retry() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/GetAgent",
        Reply::error_with_details(
            StatusCode::NOT_IMPLEMENTED,
            "unknown",
            proto::SdkErrorDetails {
                sdk_error_code: 9999,
                message: "from a newer bridge".to_string(),
                request_id: Some("req_new".to_string()),
                ..Default::default()
            },
        ),
    );
    let error = client_for(&bridge).get_agent("a1").await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Unknown));
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::Failed);
    assert_eq!(failure.error_kind.as_deref(), Some("Unknown"));
    assert!(!failure.retryable);
    assert!(!should_retry_in_seat(&error));
}

#[tokio::test]
async fn rate_limited_is_retryable_but_not_in_seat() {
    // Error::is_retryable says true; the seat still must not sleep
    // in-process because the drain owns the YieldBusy wait.
    let error = rpc_error(proto::SdkErrorCode::UsageLimitExceeded).await;
    assert!(error.is_retryable());
    assert_eq!(classify(&error).outcome, Outcome::Busy);
    assert!(!should_retry_in_seat(&error));
}

#[tokio::test]
async fn dead_endpoint_is_a_retryable_transport_startup_error() {
    let url = {
        let bridge = FakeBridge::start().await;
        bridge.url.clone()
    }; // bridge dropped: connection refused
    let client = cursor_sdk::Client::builder()
        .api_key("test-api-key")
        .endpoint(&url, "test-bridge-token")
        .build();
    let error = client.ping().await.unwrap_err();
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::StartupError);
    assert_eq!(failure.error_kind, None);
    assert_eq!(failure.request_id, None);
    assert!(failure.retryable);
    assert!(should_retry_in_seat(&error));
}

#[tokio::test]
async fn missing_bridge_binary_is_a_non_retryable_startup_error() {
    let client = cursor_sdk::Client::builder()
        .api_key("test-api-key")
        .bridge_binary("/nonexistent-cursor-sdk-bridge-bin")
        .build();
    let error = client.ping().await.unwrap_err();
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::StartupError);
    assert_eq!(failure.error_kind, None);
    assert!(!failure.retryable);
    assert!(!should_retry_in_seat(&error));
}

#[tokio::test]
async fn closed_client_is_a_bounce() {
    let bridge = FakeBridge::start().await;
    let client = client_for(&bridge);
    client.close().await.unwrap();
    let error = client.ping().await.unwrap_err();
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::Bounced);
    assert!(!failure.retryable);
}

#[tokio::test]
async fn undecodable_rpc_is_a_startup_error() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/GetAgent",
        Reply::Unary(bytes::Bytes::from_static(b"\xff\xff\xff not protobuf")),
    );
    let error = client_for(&bridge).get_agent("a1").await.unwrap_err();
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::StartupError);
    assert!(!should_retry_in_seat(&error));
}

#[tokio::test]
async fn run_status_maps_without_a_bridge() {
    use cursor_sdk::RunStatus as Status;
    assert_eq!(classify_run_status(Status::Finished), Some(Outcome::Ok));
    assert_eq!(
        classify_run_status(Status::Cancelled),
        Some(Outcome::Failed)
    );
    // A slow Timeout never becomes an outcome here; it stays retryable.
    let timeout = cursor_sdk::Error::Timeout {
        operation: "test",
        timeout: Duration::from_secs(1),
    };
    assert!(should_retry_in_seat(&timeout));
    assert_eq!(classify(&timeout).outcome, Outcome::StartupError);
}

#[tokio::test]
async fn local_agent_without_model_is_a_bounce() {
    // AgentOptions::local without .model() cannot be asked of the bridge.
    let bridge = FakeBridge::start().await;
    let error = client_for(&bridge)
        .create_agent(AgentOptions::local("/repo"))
        .await
        .unwrap_err();
    let failure = classify(&error);
    assert_eq!(failure.outcome, Outcome::Bounced);
    assert!(!failure.retryable);
}
