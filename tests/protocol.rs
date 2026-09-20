//! End-to-end tests against an in-process stand-in for the bridge.
//!
//! These drive the real transport: real Connect framing, real protobuf
//! encoding, real HTTP. Only the bridge's *behaviour* is scripted, so a bug in
//! this crate's wire handling fails a test here rather than in production.

mod support;

use cursor_sdk::{
    AgentOptions, Client, ErrorKind, ListAgents, RunEvent, RunStatus, RuntimeFilter, SendOptions,
};
use hyper::StatusCode;
use serde_json::json;
use support::*;

use cursor_sdk::proto;

// ---- handshake and control ------------------------------------------------

#[tokio::test]
async fn ping_and_version_work_over_the_real_transport() {
    let bridge = FakeBridge::start().await;
    let client = client_for(&bridge);

    assert_eq!(client.ping().await.unwrap(), "pong");

    let version = client.version().await.unwrap();
    assert_eq!(version.bridge_version, "1.0.0-test");
    assert!(version.speaks_supported_protocol());
    assert!(version.has_capability("agent.usage"));
    assert!(!version.has_capability("something.new"));
}

#[tokio::test]
async fn the_bearer_token_is_sent_on_every_rpc() {
    let bridge = FakeBridge::start().await;
    // A client with the wrong token must fail, and fail as an auth error.
    let client = Client::builder()
        .api_key("test-api-key")
        .endpoint(&bridge.url, "the-wrong-token")
        .build();

    let error = client.ping().await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Unauthenticated));
    assert!(error.is_auth());
    assert!(!error.is_retryable(), "a bad token is not worth retrying");
}

#[tokio::test]
async fn a_missing_token_also_fails_a_stream() {
    let bridge = FakeBridge::start().await;
    let client = Client::builder()
        .api_key("k")
        .endpoint(&bridge.url, "wrong")
        .verify_on_connect(false)
        .build();

    // Streams are the classic place a token gets dropped, because interceptor
    // APIs often only cover unary calls.
    let error = client.observe_run("run_1", None).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Unauthenticated));
}

#[tokio::test]
async fn connecting_verifies_the_handshake_once() {
    let bridge = FakeBridge::start().await;
    let client = client_for(&bridge);

    client.ping().await.unwrap();
    client.ping().await.unwrap();

    // One verification Ping plus two explicit ones; GetVersion only once.
    assert_eq!(bridge.call_count("SdkBridgeControlService/GetVersion"), 1);
    assert_eq!(bridge.call_count("SdkBridgeControlService/Ping"), 3);
}

// ---- catalog --------------------------------------------------------------

#[tokio::test]
async fn catalog_calls_carry_the_api_key_on_the_request() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![proto::SdkModel {
                id: "composer-2".into(),
                display_name: "Composer 2".into(),
                variants: vec![proto::ModelVariant {
                    display_name: "fast".into(),
                    is_default: true,
                    params: vec![proto::ModelParameterValue {
                        id: "speed".into(),
                        value: "fast".into(),
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }),
    );
    let client = client_for(&bridge);

    let models = client.models().await.unwrap();
    assert_eq!(models[0].id, "composer-2");
    assert_eq!(models[0].choice().id, "composer-2");
    assert!(models[0].variants[0].is_default);

    // Catalog RPCs hard-require the key; the bridge does not fall back to env.
    let request: proto::ListModelsRequest = bridge.request("SdkCursorService/ListModels");
    assert_eq!(request.options.unwrap().api_key, "test-api-key");
}

#[tokio::test]
async fn me_and_repositories_map_onto_plain_types() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/Me",
        Reply::unary(&proto::MeResponse {
            user: Some(proto::SdkUser {
                api_key_name: "ci".into(),
                user_id: 42,
                user_email: "dev@example.com".into(),
                ..Default::default()
            }),
        }),
    );
    bridge.expect(
        "SdkCursorService/ListRepositories",
        Reply::unary(&proto::ListRepositoriesResponse {
            items: vec![proto::SdkRepository {
                url: "https://github.com/acme/repo".into(),
            }],
        }),
    );
    let client = client_for(&bridge);

    assert_eq!(client.me().await.unwrap().email, "dev@example.com");
    assert_eq!(
        client.repositories().await.unwrap()[0].url,
        "https://github.com/acme/repo"
    );
}

// ---- errors ---------------------------------------------------------------

#[tokio::test]
async fn structured_error_details_are_decoded_and_classified() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::error_with_details(
            StatusCode::TOO_MANY_REQUESTS,
            "resource_exhausted",
            proto::SdkErrorDetails {
                request_id: Some("req_0123456789abcdef".into()),
                sdk_error_code: proto::SdkErrorCode::RateLimitExceeded as i32,
                message: "slow down".into(),
                retry_after: Some(prost_types::Duration {
                    seconds: 30,
                    nanos: 0,
                }),
                rate_limit: Some(proto::RateLimitInfo {
                    limit: Some(100),
                    remaining: Some(0),
                    reset_epoch_seconds: Some(1_700_000_000),
                }),
                ..Default::default()
            },
        ),
    );
    let client = client_for(&bridge);

    let error = client
        .create_agent(AgentOptions::local("/repo").model("composer-2"))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), Some(ErrorKind::RateLimited));
    assert!(error.is_retryable());
    // The full request id must survive, untruncated: support traces by it.
    assert_eq!(error.request_id(), Some("req_0123456789abcdef"));
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(30))
    );

    let cursor_sdk::Error::Rpc(rpc) = error else {
        panic!("expected an RPC error");
    };
    assert_eq!(rpc.message, "slow down");
    assert_eq!(rpc.rate_limit.unwrap().remaining, Some(0));
    assert_eq!(
        rpc.sdk_error_code_name(),
        Some("SDK_ERROR_CODE_RATE_LIMIT_EXCEEDED")
    );
    assert_eq!(rpc.rpc, "SdkAgentService/CreateAgent");
}

#[tokio::test]
async fn each_taxonomy_code_maps_to_a_kind() {
    let cases = [
        (proto::SdkErrorCode::AgentNotFound, ErrorKind::NotFound),
        (proto::SdkErrorCode::InvalidModel, ErrorKind::Validation),
        (proto::SdkErrorCode::AgentBusy, ErrorKind::AgentBusy),
        (proto::SdkErrorCode::AgentArchived, ErrorKind::InvalidState),
        (
            proto::SdkErrorCode::RunNotCancellable,
            ErrorKind::InvalidState,
        ),
        (
            proto::SdkErrorCode::PlanRequired,
            ErrorKind::PermissionDenied,
        ),
        (
            proto::SdkErrorCode::Unauthorized,
            ErrorKind::Unauthenticated,
        ),
        (proto::SdkErrorCode::UpstreamError, ErrorKind::Upstream),
        (proto::SdkErrorCode::ClientCancelled, ErrorKind::Cancelled),
    ];

    for (code, expected) in cases {
        let bridge = FakeBridge::start().await;
        bridge.expect(
            "SdkAgentService/GetAgent",
            Reply::error_with_details(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                proto::SdkErrorDetails {
                    sdk_error_code: code as i32,
                    message: format!("{code:?}"),
                    ..Default::default()
                },
            ),
        );
        let error = client_for(&bridge).get_agent("a1").await.unwrap_err();
        assert_eq!(error.kind(), Some(expected), "for {code:?}");
    }
}

#[tokio::test]
async fn an_rpc_with_no_reply_surfaces_as_an_error_not_a_hang() {
    let bridge = FakeBridge::start().await;
    let error = client_for(&bridge).get_agent("a1").await.unwrap_err();
    assert!(error.to_string().contains("no reply scripted"));
}

// ---- agents ---------------------------------------------------------------

#[tokio::test]
async fn creating_an_agent_sends_the_options_the_bridge_needs() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: Some(proto::ModelSelection {
                id: "composer-2".into(),
                params: vec![],
            }),
        }),
    );
    let client = client_for(&bridge);

    let agent = client
        .create_agent(
            AgentOptions::local("/repo")
                .model(cursor_sdk::ModelChoice::new("composer-2").with_param("reasoning", "high"))
                .name("test agent"),
        )
        .await
        .unwrap();

    assert_eq!(agent.id(), "agent_1");
    assert_eq!(agent.model().unwrap().id, "composer-2");
    assert_eq!(agent.cwd().unwrap().to_str(), Some("/repo"));

    let request: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    let options = request.options.unwrap();
    assert_eq!(
        options.api_key, "test-api-key",
        "the key must be on the options"
    );
    assert_eq!(options.model.as_ref().unwrap().id, "composer-2");
    assert_eq!(options.model.unwrap().params[0].value, "high");
    assert_eq!(options.local.unwrap().cwd, vec!["/repo".to_string()]);
    assert!(request.idempotency_key.is_none());
}

#[tokio::test]
async fn an_idempotency_key_is_forwarded() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    client_for(&bridge)
        .create_agent_idempotent(AgentOptions::cloud("https://github.com/acme/repo"), "key-1")
        .await
        .unwrap();

    let request: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    assert_eq!(request.idempotency_key.as_deref(), Some("key-1"));
}

#[tokio::test]
async fn listing_agents_follows_pagination_cursors() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/ListAgents",
        Reply::unary(&proto::ListAgentsResponse {
            items: vec![proto::SdkAgentInfo {
                agent_id: "a1".into(),
                name: "first".into(),
                status: proto::AgentInfoStatus::Finished as i32,
                runtime_info: Some(proto::sdk_agent_info::RuntimeInfo::Local(
                    proto::LocalAgentInfo {
                        cwd: "/repo".into(),
                    },
                )),
                ..Default::default()
            }],
            next_cursor: "page-2".into(),
        }),
    );
    bridge.expect(
        "SdkAgentService/ListAgents",
        Reply::unary(&proto::ListAgentsResponse {
            items: vec![proto::SdkAgentInfo {
                agent_id: "a2".into(),
                archived: true,
                ..Default::default()
            }],
            next_cursor: String::new(),
        }),
    );
    let client = client_for(&bridge);

    let all = client
        .list_all_agents(ListAgents::new().limit(1).include_archived(true))
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, "a1");
    assert!(matches!(
        all[0].runtime,
        cursor_sdk::AgentRuntime::Local { .. }
    ));
    assert_eq!(all[0].status, cursor_sdk::AgentStatus::Finished);
    assert!(all[1].archived);

    // The second page must have carried the cursor from the first.
    let bodies = bridge.recorded.lock().unwrap();
    let second = &bodies.bodies["SdkAgentService/ListAgents"][1];
    let request = <proto::ListAgentsRequest as prost::Message>::decode(&second[..]).unwrap();
    assert_eq!(request.options.unwrap().cursor, "page-2");
}

#[tokio::test]
async fn lifecycle_calls_route_by_cwd() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    for rpc in ["ArchiveAgent", "UnarchiveAgent", "DeleteAgent"] {
        bridge.always(
            &format!("SdkAgentService/{rpc}"),
            Reply::Unary(bytes::Bytes::new()),
        );
    }
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::Unary(bytes::Bytes::new()),
    );

    let client = client_for(&bridge);
    let agent = client
        .create_agent(AgentOptions::local("/repo").model("composer-2"))
        .await
        .unwrap();

    agent.archive().await.unwrap();
    agent.unarchive().await.unwrap();
    agent.close().await.unwrap();
    agent.delete().await.unwrap();

    let request: proto::ArchiveAgentRequest = bridge.request("SdkAgentService/ArchiveAgent");
    let options = request.options.unwrap();
    assert_eq!(options.cwd, "/repo");
    assert_eq!(options.api_key, "test-api-key");
}

// ---- streaming ------------------------------------------------------------

fn scripted_run_stream() -> Vec<bytes::Bytes> {
    vec![
        // An idle stream sends these roughly every 15 seconds.
        keepalive_frame(),
        sdk_message_frame(
            "system",
            json!({"subtype": "init", "run_id": "run_1", "agent_id": "agent_1"}),
            Some("offset-1"),
        ),
        sdk_message_frame(
            "assistant",
            json!({"message": {"content": [{"type": "text", "text": "Hello"}]}}),
            Some("offset-2"),
        ),
        // A case from a future contract version.
        unknown_envelope_frame(),
        sdk_message_frame("thinking", json!({"text": "hmm"}), Some("offset-3")),
        sdk_message_frame(
            "tool_call",
            json!({"status": "completed", "name": "read_file"}),
            Some("offset-4"),
        ),
        sdk_message_frame(
            "assistant",
            json!({"message": {"content": [{"type": "text", "text": ", world."}]}}),
            Some("offset-5"),
        ),
        keepalive_frame(),
        result_frame(
            "agent_1",
            "run_1",
            proto::RunLifecycleStatus::Finished,
            "Hello, world.",
        ),
        done_frame("agent_1", "run_1"),
    ]
}

async fn agent_on(bridge: &FakeBridge) -> cursor_sdk::Agent {
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    client_for(bridge)
        .create_agent(AgentOptions::local("/repo").model("composer-2"))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_turn_streams_events_and_ends_with_a_result() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect("SdkAgentService/Send", Reply::Stream(scripted_run_stream()));

    let mut run = agent.send("hi").await.unwrap();
    let mut kinds = Vec::new();
    let mut text = String::new();
    let mut outcome = None;

    while let Some(event) = run.next_event().await {
        match event.unwrap() {
            RunEvent::Message(message) => {
                kinds.push(message.kind.clone());
                if let Some(chunk) = message.is_assistant().then(|| message.text()).flatten() {
                    text.push_str(&chunk);
                }
            }
            RunEvent::Completed(result) => outcome = Some(result),
            other => panic!("unexpected event {other:?}"),
        }
    }

    // Keepalives and the unknown envelope never surfaced as events.
    assert_eq!(
        kinds,
        vec!["system", "assistant", "thinking", "tool_call", "assistant"]
    );
    assert_eq!(text, "Hello, world.");

    let outcome = outcome.expect("a terminal result");
    assert_eq!(outcome.status, RunStatus::Finished);
    assert!(outcome.status.is_terminal() && outcome.status.is_success());
    assert_eq!(outcome.text, "Hello, world.");
    assert_eq!(outcome.duration, std::time::Duration::from_millis(1234));
    assert_eq!(run.run_id(), Some("run_1"));
    assert_eq!(
        run.last_offset(),
        Some("offset-5"),
        "keepalives carry no offset"
    );
}

#[tokio::test]
async fn next_text_yields_only_assistant_output() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect("SdkAgentService/Send", Reply::Stream(scripted_run_stream()));

    let mut run = agent.send("hi").await.unwrap();
    let mut chunks = Vec::new();
    while let Some(chunk) = run.next_text().await {
        chunks.push(chunk.unwrap());
    }
    assert_eq!(chunks, vec!["Hello".to_string(), ", world.".to_string()]);
}

#[tokio::test]
async fn wait_returns_the_outcome_without_manual_iteration() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect("SdkAgentService/Send", Reply::Stream(scripted_run_stream()));

    assert_eq!(agent.ask("hi").await.unwrap(), "Hello, world.");
}

#[tokio::test]
async fn send_options_reach_the_wire() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect("SdkAgentService/Send", Reply::Stream(scripted_run_stream()));

    agent
        .send_with(
            "hi",
            SendOptions::new()
                .deltas(true)
                .steps(true)
                .model("composer-2")
                .force(true),
        )
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();

    let request: proto::SendRequest = bridge.stream_request("SdkAgentService/Send");
    let options = request.options.unwrap();
    assert!(options.enable_deltas && options.enable_steps);
    assert_eq!(options.local.unwrap().force, Some(true));
    assert_eq!(request.message.unwrap().text, "hi");
}

#[tokio::test]
async fn deltas_and_steps_surface_as_their_own_events() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            frame(&proto::RunStreamMessage {
                offset: None,
                envelope: Some(proto::run_stream_message::Envelope::InteractionUpdate(
                    proto::InteractionUpdate {
                        r#type: "text-delta".into(),
                        update: cursor_sdk::json::json_to_struct(&json!({"delta": "He"})),
                    },
                )),
            }),
            frame(&proto::RunStreamMessage {
                offset: None,
                envelope: Some(proto::run_stream_message::Envelope::Step(
                    proto::ConversationStep {
                        r#type: "tool-step".into(),
                        step: cursor_sdk::json::json_to_struct(&json!({"name": "read"})),
                    },
                )),
            }),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "ok",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );

    let mut run = agent
        .send_with("hi", SendOptions::new().deltas(true).steps(true))
        .await
        .unwrap();

    let mut saw_delta = false;
    let mut saw_step = false;
    while let Some(event) = run.next_event().await {
        match event.unwrap() {
            RunEvent::Delta { kind, payload } => {
                assert_eq!(kind, "text-delta");
                assert_eq!(payload["delta"], json!("He"));
                saw_delta = true;
            }
            RunEvent::Step { kind, .. } => {
                assert_eq!(kind, "tool-step");
                saw_step = true;
            }
            _ => {}
        }
    }
    assert!(saw_delta && saw_step);
}

#[tokio::test]
async fn a_failed_run_is_not_an_rpc_error_and_keeps_its_reason() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "system",
                json!({"subtype": "init", "run_id": "run_1"}),
                Some("o1"),
            ),
            // On failure the readable reason arrives here, and error_code on
            // the terminal result is often empty.
            sdk_message_frame(
                "status",
                json!({"status": "error", "message": "the model ran out of context"}),
                Some("o2"),
            ),
            frame(&proto::RunStreamMessage {
                offset: None,
                envelope: Some(proto::run_stream_message::Envelope::Result(
                    proto::RunStreamResult {
                        agent_id: "agent_1".into(),
                        run_id: "run_1".into(),
                        status: proto::RunLifecycleStatus::Error as i32,
                        error_code: None,
                        result: None,
                    },
                )),
            }),
            done_frame("agent_1", "run_1"),
        ]),
    );

    let outcome = agent.send("hi").await.unwrap().wait().await.unwrap();
    assert_eq!(outcome.status, RunStatus::Error);
    assert!(!outcome.status.is_success());
    assert_eq!(
        outcome.failure_reason().as_deref(),
        Some("the model ran out of context"),
        "the status message is the only readable reason here"
    );
}

#[tokio::test]
async fn text_fails_loudly_when_the_run_did_not_succeed() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "status",
                json!({"message": "cancelled by user"}),
                Some("o1"),
            ),
            result_frame("agent_1", "run_1", proto::RunLifecycleStatus::Cancelled, ""),
            done_frame("agent_1", "run_1"),
        ]),
    );

    let error = agent.send("hi").await.unwrap().text().await.unwrap_err();
    assert!(error.to_string().contains("cancelled by user"));
}

#[tokio::test]
async fn a_stream_error_frame_becomes_a_classified_error() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    // A stream that fails mid-flight: HTTP 200, error in the end frame.
    let mut frames = vec![sdk_message_frame(
        "system",
        json!({"run_id": "run_1"}),
        Some("o1"),
    )];
    frames.push(end_of_stream(Some(json!({
        "code": "not_found",
        "message": "that agent is gone",
    }))));
    bridge.expect("SdkAgentService/Send", Reply::Stream(frames));

    let mut run = agent.send("hi").await.unwrap();
    run.next_event().await.unwrap().unwrap(); // the system message
    let error = run.next_event().await.unwrap().unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::NotFound));
    assert!(error.is_not_found());
}

#[tokio::test]
async fn a_dropped_stream_falls_back_to_wait_live_run() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    // The stream ends after revealing the run id but before a result.
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![sdk_message_frame(
            "system",
            json!({"run_id": "run_1", "agent_id": "agent_1"}),
            Some("o1"),
        )]),
    );
    bridge.expect(
        "SdkAgentService/WaitLiveRun",
        Reply::unary(&proto::WaitLiveRunResponse {
            result: Some(proto::RunResult {
                run_id: "run_1".into(),
                agent_id: "agent_1".into(),
                status: proto::RunLifecycleStatus::Finished as i32,
                result: "recovered".into(),
                ..Default::default()
            }),
        }),
    );

    // Dropping the Send stream does not cancel the run, so the outcome is
    // still recoverable.
    let outcome = agent.send("hi").await.unwrap().wait().await.unwrap();
    assert_eq!(outcome.text, "recovered");
    assert_eq!(bridge.call_count("SdkAgentService/WaitLiveRun"), 1);
}

#[tokio::test]
async fn resuming_a_live_stream_replays_from_the_beginning() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![sdk_message_frame(
            "system",
            json!({"run_id": "run_1"}),
            Some("live-offset-7"),
        )]),
    );
    bridge.expect(
        "SdkAgentService/ObserveRun",
        Reply::Stream(vec![
            sdk_message_frame("assistant", json!({"text": "replayed"}), Some("durable-1")),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "done",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );

    let mut run = agent.send("hi").await.unwrap();
    run.next_event().await.unwrap().unwrap();
    assert_eq!(run.last_offset(), Some("live-offset-7"));

    run.resume().await.unwrap();

    // A live Send offset is NOT a valid ObserveRun resume point: passing one
    // can silently skip durable events, so the replay starts from scratch.
    let request: proto::ObserveRunRequest = bridge.stream_request("SdkAgentService/ObserveRun");
    assert_eq!(request.run_id, "run_1");
    assert_eq!(request.after_offset, None);

    let outcome = run.wait().await.unwrap();
    assert_eq!(outcome.text, "done");
}

#[tokio::test]
async fn resuming_a_durable_stream_uses_its_own_offset() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/ObserveRun",
        Reply::Stream(vec![sdk_message_frame(
            "assistant",
            json!({"text": "a"}),
            Some("durable-3"),
        )]),
    );
    bridge.expect(
        "SdkAgentService/ObserveRun",
        Reply::Stream(vec![
            result_frame("agent_1", "run_1", proto::RunLifecycleStatus::Finished, "z"),
            done_frame("agent_1", "run_1"),
        ]),
    );
    let client = client_for(&bridge);

    let mut run = client.observe_run("run_1", None).await.unwrap();
    run.next_event().await.unwrap().unwrap();
    run.resume().await.unwrap();

    let recorded = bridge.recorded.lock().unwrap();
    let second = &recorded.bodies["SdkAgentService/ObserveRun"][1];
    let request = <proto::ObserveRunRequest as prost::Message>::decode(&second[5..]).unwrap();
    assert_eq!(
        request.after_offset.as_deref(),
        Some("durable-3"),
        "offsets from ObserveRun are valid ObserveRun resume points"
    );
}

#[tokio::test]
async fn cancelling_a_run_names_the_agent() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame("system", json!({"run_id": "run_1"}), Some("o1")),
            result_frame("agent_1", "run_1", proto::RunLifecycleStatus::Cancelled, ""),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );

    let mut run = agent.send("hi").await.unwrap();
    run.next_event().await.unwrap().unwrap();
    run.cancel().await.unwrap();

    let request: proto::CancelRunRequest = bridge.request("SdkAgentService/CancelRun");
    assert_eq!(request.run_id, "run_1");
    assert_eq!(request.agent_id.as_deref(), Some("agent_1"));

    // Cancellation still delivers a terminal result.
    let outcome = run.wait().await.unwrap();
    assert_eq!(outcome.status, RunStatus::Cancelled);
}

#[tokio::test]
async fn cancelling_before_the_run_id_arrives_is_a_clear_error() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect("SdkAgentService/Send", Reply::Stream(vec![]));

    let run = agent.send("hi").await.unwrap();
    let error = run.cancel().await.unwrap_err();
    assert!(error.to_string().contains("no id yet"));
}

// ---- artifacts, usage, conversation ---------------------------------------

#[tokio::test]
async fn artifacts_download_in_chunks() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/ListArtifacts",
        Reply::unary(&proto::ListArtifactsResponse {
            artifacts: vec![proto::SdkArtifact {
                path: "out/report.txt".into(),
                size_bytes: 11,
                updated_at: "2026-01-01T00:00:00Z".into(),
            }],
        }),
    );
    bridge.expect(
        "SdkAgentService/DownloadArtifact",
        Reply::Stream(vec![
            frame(&proto::DownloadArtifactChunk {
                data: bytes::Bytes::from_static(b"hello "),
            }),
            frame(&proto::DownloadArtifactChunk {
                data: bytes::Bytes::from_static(b"world"),
            }),
        ]),
    );

    let artifacts = agent.artifacts().await.unwrap();
    assert_eq!(artifacts[0].size_bytes, 11);

    let bytes = agent.download_artifact("out/report.txt").await.unwrap();
    assert_eq!(bytes, b"hello world");
}

#[tokio::test]
async fn artifacts_can_stream_straight_to_a_file() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/DownloadArtifact",
        Reply::Stream(vec![frame(&proto::DownloadArtifactChunk {
            data: bytes::Bytes::from_static(b"payload"),
        })]),
    );

    let directory =
        std::env::temp_dir().join(format!("cursor-sdk-artifact-{}", std::process::id()));
    let destination = directory.join("nested/report.txt");
    let written = agent
        .download_artifact_to("out/report.txt", &destination)
        .await
        .unwrap();

    assert_eq!(written, 7);
    assert_eq!(std::fs::read(&destination).unwrap(), b"payload");
    std::fs::remove_dir_all(&directory).unwrap();
}

#[tokio::test]
async fn usage_totals_and_per_run_breakdowns_map_over() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse {
            usage: Some(proto::AgentUsage {
                usage: Some(proto::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    total_tokens: 120,
                    reasoning_tokens: Some(5),
                    ..Default::default()
                }),
                cost: Some(proto::UsageCost {
                    raw_cost_cents: 1.5,
                    charged_cents: 1.0,
                }),
                runs: vec![proto::RunUsage {
                    run_id: "run_1".into(),
                    usage: Some(proto::TokenUsage {
                        total_tokens: 120,
                        ..Default::default()
                    }),
                    cost: None,
                }],
            }),
        }),
    );

    let usage = agent.usage().await.unwrap();
    assert_eq!(usage.usage.total_tokens, 120);
    assert_eq!(usage.usage.reasoning_tokens, Some(5));
    assert_eq!(usage.cost.unwrap().charged_cents, 1.0);
    assert_eq!(usage.runs[0].run_id, "run_1");
    assert!(usage.runs[0].cost.is_none());
}

#[tokio::test]
async fn the_conversation_document_comes_back_as_json() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/GetRunConversation",
        Reply::unary(&proto::GetRunConversationResponse {
            conversation_json: r#"{"messages":[{"role":"user"}]}"#.into(),
        }),
    );
    let value = client_for(&bridge).run_conversation("run_1").await.unwrap();
    assert_eq!(value["messages"][0]["role"], json!("user"));
}

#[tokio::test]
async fn runs_and_messages_list_for_an_agent() {
    let bridge = FakeBridge::start().await;
    let agent = agent_on(&bridge).await;
    bridge.expect(
        "SdkAgentService/ListRuns",
        Reply::unary(&proto::ListRunsResponse {
            items: vec![proto::RunSnapshot {
                run_id: "run_1".into(),
                agent_id: "agent_1".into(),
                status: proto::RunLifecycleStatus::Finished as i32,
                result: "done".into(),
                ..Default::default()
            }],
            next_cursor: String::new(),
        }),
    );
    bridge.expect(
        "SdkAgentService/ListAgentMessages",
        Reply::unary(&proto::ListAgentMessagesResponse {
            messages: vec![proto::AgentMessage {
                r#type: "assistant".into(),
                uuid: "m1".into(),
                agent_id: "agent_1".into(),
                message: cursor_sdk::json::json_to_struct(&json!({"text": "hi"})),
            }],
        }),
    );

    let runs = agent
        .runs(cursor_sdk::ListRuns::new().limit(5))
        .await
        .unwrap();
    assert!(!runs.has_more());
    assert_eq!(runs.items[0].status, RunStatus::Finished);

    let messages = agent
        .messages(cursor_sdk::ListMessages::new().limit(10))
        .await
        .unwrap();
    assert_eq!(messages[0].kind, "assistant");
    assert_eq!(messages[0].payload["text"], json!("hi"));
}

#[tokio::test]
async fn the_runtime_filter_is_encoded() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/ListAgents",
        Reply::unary(&proto::ListAgentsResponse::default()),
    );
    client_for(&bridge)
        .list_agents(ListAgents::new().runtime(RuntimeFilter::Local).cwd("/repo"))
        .await
        .unwrap();

    let request: proto::ListAgentsRequest = bridge.request("SdkAgentService/ListAgents");
    let options = request.options.unwrap();
    assert_eq!(options.runtime, proto::Runtime::Local as i32);
    assert_eq!(options.cwd, "/repo");
}

// ---- one-liner ------------------------------------------------------------

#[tokio::test]
async fn prompt_creates_sends_waits_and_closes() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    bridge.expect("SdkAgentService/Send", Reply::Stream(scripted_run_stream()));
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );

    let answer = client_for(&bridge)
        .prompt(AgentOptions::local("/repo").model("composer-2"), "hi")
        .await
        .unwrap();

    assert_eq!(answer, "Hello, world.");
    assert_eq!(bridge.call_count("SdkAgentService/CloseAgent"), 1);
}

#[tokio::test]
async fn prompt_closes_the_agent_even_when_the_run_fails() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    bridge.expect(
        "SdkAgentService/Send",
        Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "boom"),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );

    let error = client_for(&bridge)
        .prompt(AgentOptions::local("/repo").model("composer-2"), "hi")
        .await
        .unwrap_err();

    assert!(error.to_string().contains("boom"));
    assert_eq!(
        bridge.call_count("SdkAgentService/CloseAgent"),
        1,
        "a failed turn must not leak the agent"
    );
}
