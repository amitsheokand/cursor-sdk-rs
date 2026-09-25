//! P4 end-to-end tests: `run_seat` against the FakeBridge.
//!
//! Covers the happy path (event stream, usage fallback, archive),
//! cancel queued before `run_id`, resume with replay dedup, pre-start
//! busy/retry/bounce outcomes, and catalog validation.

mod support;

use cursor_sdk::proto;
use cursor_seat::inbox::Inbox;
use cursor_seat::protocol::{
    ControlInput, ControlKind, ControlMode, Limits, ModelParams, ModelRef, Outcome, PromptPart,
    SeatEvent, SeatEventKind, SeatRequest, SeatResult,
};
use cursor_seat::run::run_seat;
use serde_json::json;
use support::*;
use tokio::sync::mpsc;

fn request_with(body: &str, effort: Option<&str>, timeout_s: u64) -> (TestDir, SeatRequest) {
    let cwd = workspace_dir();
    let req = request_with_cwd(body, effort, timeout_s, &cwd);
    (cwd, req)
}

fn request_with_cwd(
    body: &str,
    effort: Option<&str>,
    timeout_s: u64,
    cwd: &std::path::Path,
) -> SeatRequest {
    SeatRequest {
        v: 1,
        request_id: "pkt-1:1".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: ModelRef {
            id: "composer-2.5".into(),
            params: ModelParams {
                effort: effort.map(str::to_string),
                extra: Default::default(),
            },
        },
        prompt: PromptPart {
            task: "do it".into(),
            effort_tag: "high".into(),
            body: body.into(),
            steer: None,
        },
        mcp_servers: vec![],
        disallowed_tools: vec!["task".into()],
        tools_enabled: vec![],
        skill_roots: vec![],
        jev: Default::default(),
        toolgate: Default::default(),
        limits: Limits {
            context_chars: 100_000,
            clip_chars: 40_000,
            timeout_s,
            heartbeat_s: 30,
        },
        session_dir: None,
        fence: vec![],
        protected_roots: vec![],
    }
}

fn effort_value(value: &str) -> proto::ModelParameterDefinitionValue {
    proto::ModelParameterDefinitionValue {
        value: value.into(),
        display_name: value.into(),
    }
}

fn composer_model() -> proto::SdkModel {
    proto::SdkModel {
        id: "composer-2.5".into(),
        display_name: "Composer".into(),
        parameters: vec![proto::ModelParameterDefinition {
            id: "effort".into(),
            display_name: "Effort".into(),
            values: vec![effort_value("low"), effort_value("high")],
        }],
        ..Default::default()
    }
}

fn script_models(bridge: &FakeBridge) {
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![composer_model()],
        }),
    );
}

fn script_create(bridge: &FakeBridge) {
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
}

fn script_close(bridge: &FakeBridge) {
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
}

fn script_usage(bridge: &FakeBridge) {
    bridge.expect(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse {
            usage: Some(proto::AgentUsage {
                usage: Some(proto::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    total_tokens: 120,
                    ..Default::default()
                }),
                runs: vec![proto::RunUsage {
                    run_id: "run_1".into(),
                    usage: Some(proto::TokenUsage {
                        input_tokens: 100,
                        output_tokens: 20,
                        total_tokens: 120,
                        ..Default::default()
                    }),
                    cost: None,
                }],
                ..Default::default()
            }),
        }),
    );
}

fn happy_stream() -> Vec<bytes::Bytes> {
    vec![
        sdk_message_frame(
            "system",
            json!({"subtype": "init", "run_id": "run_1", "agent_id": "agent_1"}),
            Some("o1"),
        ),
        sdk_message_frame(
            "assistant",
            json!({"message": {"content": [{"type": "text", "text": "Hello"}]}}),
            Some("o2"),
        ),
        sdk_message_frame(
            "tool_call",
            json!({
                "name": "mcp",
                "args": {"namespace": "fs", "toolName": "read"},
                "status": "completed",
                "call_id": "c1",
            }),
            Some("o3"),
        ),
        sdk_message_frame(
            "usage",
            json!({"input_tokens": 100, "output_tokens": 20, "total_tokens": 120}),
            Some("o4"),
        ),
        sdk_message_frame(
            "assistant",
            json!({"message": {"content": [{"type": "text", "text": ", world."}]}}),
            Some("o5"),
        ),
        result_frame(
            "agent_1",
            "run_1",
            proto::RunLifecycleStatus::Finished,
            "Hello, world.",
        ),
        done_frame("agent_1", "run_1"),
    ]
}

async fn collect(
    bridge: &FakeBridge,
    request: SeatRequest,
    inbox: Inbox,
) -> (Vec<SeatEvent>, SeatResult) {
    let client = client_for(bridge);
    let (tx, mut rx) = mpsc::unbounded_channel();
    // No spawn: `run_seat`'s future is not Send (it holds `&mut` closures
    // across awaits), so drive it inline while draining the channel.
    let run_fut = run_seat(&client, request, inbox, tx, None);
    tokio::pin!(run_fut);
    let mut events = Vec::new();
    let mut result = None;
    let mut returned = None;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if returned.is_none() => {
                returned = Some(outcome);
            }
            event = rx.recv() => match event {
                Some(event) => {
                    if let SeatEventKind::Result(failure) = &event.kind {
                        result = Some(failure.clone());
                    }
                    events.push(event);
                }
                // Sender dropped (run returned) and buffer drained.
                None => break,
            },
        }
    }
    let returned = returned.expect("run_seat returned");
    let result = result.expect("a result event");
    assert_eq!(returned, result);
    // Contiguous seq from 0.
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.seq, index as u64, "event {index}");
    }
    (events, result)
}

fn event_types(events: &[SeatEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            serde_json::to_value(event)
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn happy_path_streams_events_and_captures_usage() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    script_create(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(happy_stream()));
    script_usage(&bridge);
    script_close(&bridge);

    let (_cwd, req) = request_with("the body", Some("high"), 600);
    let (events, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert_eq!(
        event_types(&events),
        vec![
            "seat_started",
            "context_report",
            "run_started",
            "assistant",
            "tool_call",
            "usage",
            "assistant",
            "result"
        ]
    );
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(result.status, "finished");
    assert_eq!(result.error_kind, None);
    assert_eq!(result.text, "Hello, world.");
    assert_eq!(result.request_id, "pkt-1:1");
    assert_eq!(result.run_id.as_deref(), Some("run_1"));
    assert_eq!(result.agent_id.as_deref(), Some("agent_1"));
    assert_eq!(result.model.as_deref(), Some("composer-2.5"));
    // models + create + send each count as a start attempt.
    assert_eq!(result.attempts, 3);
    assert!(!result.resumed);
    assert!(result.ttfe_ms.is_some());
    // Usage fell back to the GetUsage RPC (the stream result had none).
    let usage = result.usage.unwrap();
    assert_eq!(usage.total_tokens, Some(120));
    assert_eq!(usage.input_tokens, Some(100));
    // tool_label recovered server/tool from the mcp args.
    let tool = events.iter().find_map(|event| match &event.kind {
        SeatEventKind::ToolCall { label, .. } => Some(label.clone()),
        _ => None,
    });
    assert_eq!(tool.as_deref(), Some("fs/read"));
    // Empty tools_enabled means unrestricted: no ToolList allowlist on
    // the wire (an empty list would strip all built-in tools).
    let create: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    assert!(create.options.unwrap().tools.is_none());
}

#[tokio::test]
async fn hard_cancel_before_run_id_fires_on_run_started() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    script_create(&bridge);
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "system",
                json!({"run_id": "run_1", "agent_id": "agent_1"}),
                Some("o1"),
            ),
            result_frame("agent_1", "run_1", proto::RunLifecycleStatus::Cancelled, ""),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );
    script_close(&bridge);

    // The stop arrives before any run_id exists: it must queue.
    let inbox = Inbox::new(&[]).unwrap();
    inbox
        .submit(ControlInput {
            id: "c1".into(),
            kind: ControlKind::Control,
            mode: ControlMode::Hard,
            reason: "stop".into(),
            parameters: None,
        })
        .unwrap();

    let (_cwd, req) = request_with("b", Some("high"), 600);
    let (_, result) = collect(&bridge, req, inbox).await;

    assert_eq!(bridge.call_count("SdkAgentService/CancelRun"), 1);
    let cancel: proto::CancelRunRequest = bridge.request("SdkAgentService/CancelRun");
    assert_eq!(cancel.run_id, "run_1");
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.status, "cancelled");
    // Post-start failures carry Unknown: P6 triage input (triage skips
    // status=cancelled as self-inflicted).
    assert_eq!(result.error_kind.as_deref(), Some("Unknown"));
}

#[tokio::test]
async fn dropped_stream_resumes_without_replaying_events() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    script_create(&bridge);
    // Live stream reveals the run id and one chunk, then drops (the fake
    // bridge appends an empty end-of-stream frame).
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "system",
                json!({"run_id": "run_1", "agent_id": "agent_1"}),
                Some("o1"),
            ),
            sdk_message_frame(
                "assistant",
                json!({"message": {"content": [{"type": "text", "text": "Hello"}]}}),
                Some("o2"),
            ),
        ]),
    );
    // Durable replay restarts the transcript: same chunk, then the result.
    bridge.expect(
        "SdkAgentService/ObserveRun",
        Reply::Stream(vec![
            sdk_message_frame(
                "assistant",
                json!({"message": {"content": [{"type": "text", "text": "Hello"}]}}),
                Some("d1"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "Hello",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    script_usage(&bridge);
    script_close(&bridge);

    let (_cwd, req) = request_with("b", Some("high"), 600);
    let (events, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert!(result.resumed);
    assert_eq!(result.outcome, Outcome::Ok);
    // The replayed chunk is deduped; the resume itself is announced.
    assert_eq!(
        event_types(&events),
        vec![
            "seat_started",
            "context_report",
            "run_started",
            "assistant",
            "resumed",
            "result"
        ]
    );
}

#[tokio::test]
async fn busy_create_is_terminal_without_a_send() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::error_with_details(
            hyper::StatusCode::TOO_MANY_REQUESTS,
            "resource_exhausted",
            proto::SdkErrorDetails {
                sdk_error_code: proto::SdkErrorCode::AgentBusy as i32,
                message: "agent is already running".into(),
                retry_after: Some(prost_types::Duration {
                    seconds: 5,
                    nanos: 0,
                }),
                ..Default::default()
            },
        ),
    );
    script_close(&bridge);

    let (_cwd, req) = request_with("b", Some("high"), 600);
    let (_, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert_eq!(result.outcome, Outcome::Busy);
    assert_eq!(result.error_kind.as_deref(), Some("AgentBusy"));
    assert_eq!(result.retry_after_ms, Some(5_000));
    assert_eq!(result.request_id, "pkt-1:1");
    // models + the failed create.
    assert_eq!(result.attempts, 2);
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 0);
}

#[tokio::test]
async fn transient_create_retries_on_the_same_attempt() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::error(
            hyper::StatusCode::BAD_GATEWAY,
            "unavailable",
            "upstream down",
        ),
    );
    script_create(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(happy_stream()));
    script_usage(&bridge);
    script_close(&bridge);

    let (_cwd, req) = request_with("b", Some("high"), 600);
    let (_, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    // models(1) + create fail(2) + create ok(3) + send(4).
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(result.attempts, 4);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 2);
}

#[tokio::test]
async fn unknown_model_bounces_before_any_agent_call() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse { items: vec![] }),
    );
    script_close(&bridge);

    let (_cwd, req) = request_with("b", Some("high"), 600);
    let (_, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.attempts, 1);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 0);
}

#[tokio::test]
async fn effort_outside_the_catalog_bounces() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    script_close(&bridge);

    // "medium" is not among the catalog values (low, high).
    let (_cwd, req) = request_with("b", Some("medium"), 600);
    let (_, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 0);
}

/// Live composer-2.5 shape: no effort param, only fast (+ variants).
fn fast_only_model() -> proto::SdkModel {
    proto::SdkModel {
        id: "composer-2.5".into(),
        display_name: "Composer".into(),
        parameters: vec![proto::ModelParameterDefinition {
            id: "fast".into(),
            display_name: "Fast".into(),
            values: vec![effort_value("false"), effort_value("true")],
        }],
        variants: vec![proto::ModelVariant {
            params: vec![proto::ModelParameterValue {
                id: "fast".into(),
                value: "false".into(),
            }],
            display_name: "Composer 2.5".into(),
            is_default: false,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn skipped_effort_sends_fast_only() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![fast_only_model()],
        }),
    );
    script_create(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(happy_stream()));
    script_usage(&bridge);
    script_close(&bridge);

    // "n/a" (what the drain sends for composer) skips the effort param.
    let (_cwd, req) = request_with("b", Some("n/a"), 600);
    let (_, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert_eq!(result.outcome, Outcome::Ok);
    let create: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    let model = create.options.unwrap().model.unwrap();
    assert_eq!(model.id, "composer-2.5");
    let params: std::collections::HashMap<String, String> = model
        .params
        .iter()
        .map(|param| (param.id.clone(), param.value.clone()))
        .collect();
    assert_eq!(params.get("fast").map(String::as_str), Some("false"));
    assert_eq!(params.len(), 1);
}

#[tokio::test]
async fn non_skip_effort_without_catalog_param_bounces() {
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![fast_only_model()],
        }),
    );
    script_close(&bridge);

    let (_cwd, req) = request_with("b", Some("high"), 600);
    let (_, result) = collect(&bridge, req, Inbox::new(&[]).unwrap()).await;

    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 0);
}

#[tokio::test]
async fn full_text_is_archived_when_a_session_dir_is_given() {
    let bridge = FakeBridge::start().await;
    script_models(&bridge);
    script_create(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(happy_stream()));
    script_usage(&bridge);
    script_close(&bridge);

    let dir = workspace_dir();
    let (_cwd, mut request) = request_with("b", Some("high"), 600);
    request.session_dir = Some(dir.to_string_lossy().into_owned());

    let (_, result) = collect(&bridge, request, Inbox::new(&[]).unwrap()).await;

    let path = result.archive_path.expect("archive path");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, world.");
}
