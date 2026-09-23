//! P5 at-most-once tests: replay and resume through `SessionStore`.
//!
//! Replay is proven by re-invoking against a dead endpoint: any bridge
//! contact would surface as a transport error, so an identical result
//! proves zero contact. Resume is proven by attaching with no
//! `CreateAgent`/`Send` calls recorded.

mod support;

use std::path::PathBuf;

use cursor_seat::inbox::Inbox;
use cursor_seat::protocol::{
    Limits, ModelParams, ModelRef, Outcome, PromptPart, SeatEvent, SeatEventKind, SeatRequest,
    SeatResult,
};
use cursor_seat::run::run_seat;
use cursor_seat::session::{Opened, SessionStore};
use cursor_sdk::proto;
use serde_json::json;
use support::*;
use tokio::sync::mpsc;

fn request_with_session(dir: &PathBuf) -> SeatRequest {
    SeatRequest {
        v: 1,
        request_id: "pkt-9:1".into(),
        cwd: workspace_dir().to_string_lossy().into_owned(),
        model: ModelRef {
            id: "composer-2.5".into(),
            params: ModelParams {
                effort: Some("high".into()),
                extra: Default::default(),
            },
        },
        prompt: PromptPart {
            task: "do it".into(),
            effort_tag: "high".into(),
            body: "the body".into(),
            steer: None,
        },
        mcp_servers: vec![],
        disallowed_tools: vec!["task".into()],
        tools_enabled: vec![],
        skill_roots: vec![],
        jev: Default::default(),
        limits: Limits {
            context_chars: 100_000,
            clip_chars: 40_000,
            timeout_s: 600,
            heartbeat_s: 30,
        },
        session_dir: Some(dir.display().to_string()),
        fence: vec![],
        protected_roots: vec![],
    }
}

fn composer_model() -> proto::SdkModel {
    proto::SdkModel {
        id: "composer-2.5".into(),
        display_name: "Composer".into(),
        parameters: vec![proto::ModelParameterDefinition {
            id: "effort".into(),
            display_name: "Effort".into(),
            values: vec![proto::ModelParameterDefinitionValue {
                value: "high".into(),
                display_name: "high".into(),
            }],
        }],
        ..Default::default()
    }
}

async fn invoke(
    client: &cursor_sdk::Client,
    request: SeatRequest,
    dir: &PathBuf,
) -> (Vec<SeatEvent>, SeatResult) {
    // Mirrors main.rs: open the store, seed the inbox, run.
    let (store, _) = SessionStore::open(dir, &request.request_id).unwrap();
    let seen = store.seen_control_ids().to_vec();
    let inbox = Inbox::new(&seen).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(client, request, inbox, tx, Some(store));
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
                None => break,
            },
        }
    }
    (events, result.expect("a result event"))
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

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("seat-p5-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn terminal_result_replays_with_no_bridge_contact() {
    let dir = tempdir("replay");
    let request = request_with_session(&dir);

    // First invocation: pre-start busy, recorded as terminal.
    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![composer_model()],
        }),
    );
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::error_with_details(
            hyper::StatusCode::TOO_MANY_REQUESTS,
            "resource_exhausted",
            proto::SdkErrorDetails {
                sdk_error_code: proto::SdkErrorCode::AgentBusy as i32,
                message: "busy".into(),
                ..Default::default()
            },
        ),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    let (first_events, first) = invoke(&client_for(&bridge), request.clone(), &dir).await;
    assert_eq!(first.outcome, Outcome::Busy);
    assert_eq!(first.request_id, "pkt-9:1");
    assert_eq!(
        event_types(&first_events),
        vec!["seat_started", "context_report", "result"]
    );

    // Second invocation against a dead endpoint: any RPC would fail, so
    // an identical result proves the replay made zero bridge contact.
    let dead_url = {
        let dead = FakeBridge::start().await;
        dead.url.clone()
    };
    let dead = cursor_sdk::Client::builder()
        .api_key("test-api-key")
        .endpoint(&dead_url, "test-bridge-token")
        .build();
    let (events, second) = invoke(&dead, request, &dir).await;
    assert_eq!(
        event_types(&events),
        vec!["seat_started", "context_report", "result"]
    );
    assert_eq!(second.outcome, first.outcome);
    assert_eq!(second.status, first.status);
    assert_eq!(second.error_kind, first.error_kind);
    assert_eq!(second.request_id, first.request_id);
    assert_eq!(second.attempts, first.attempts);
    assert_eq!(second.text, first.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn live_run_attaches_instead_of_resending() {
    let dir = tempdir("resume");
    // Simulate a sender that opened the stream then died: awaiting + run
    // identity, no result.
    let (mut store, opened) = SessionStore::open(&dir, "pkt-9:1").unwrap();
    assert_eq!(opened, Opened::Fresh);
    store
        .set_state(cursor_seat::session::OpState::Awaiting)
        .unwrap();
    store.record_run("run_9", "agent_9").unwrap();
    drop(store);

    let bridge = FakeBridge::start().await;
    // No ListModels/CreateAgent/Send scripts: any of those RPCs would
    // fail the test with "no reply scripted".
    bridge.expect(
        "SdkAgentService/ObserveRun",
        Reply::Stream(vec![
            sdk_message_frame(
                "assistant",
                json!({"message": {"content": [{"type": "text", "text": "Hi"}]}}),
                Some("d1"),
            ),
            result_frame(
                "agent_9",
                "run_9",
                proto::RunLifecycleStatus::Finished,
                "Hi",
            ),
            done_frame("agent_9", "run_9"),
        ]),
    );

    let (events, result) = invoke(&client_for(&bridge), request_with_session(&dir), &dir).await;

    assert_eq!(bridge.call_count("SdkAgentService/Send"), 0);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 0);
    assert_eq!(bridge.call_count("SdkCursorService/ListModels"), 0);
    assert_eq!(
        event_types(&events),
        vec![
            "seat_started",
            "context_report",
            "run_started",
            "assistant",
            "result"
        ]
    );
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(result.run_id.as_deref(), Some("run_9"));
    assert_eq!(result.agent_id.as_deref(), Some("agent_9"));
    assert!(result.resumed);
    assert_eq!(result.text, "Hi");
    // The attach recorded its terminal state: a third invoke replays.
    let (store, opened) = SessionStore::open(&dir, "pkt-9:1").unwrap();
    assert_eq!(opened, Opened::Replay);
    assert_eq!(store.stored_result().unwrap().text, "Hi");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ready_session_sends_normally_and_records() {
    let dir = tempdir("fresh");
    // Open (session record only) then run: op is Ready, so this sends.
    let (store, opened) = SessionStore::open(&dir, "pkt-9:1").unwrap();
    assert_eq!(opened, Opened::Fresh);
    drop(store);

    let bridge = FakeBridge::start().await;
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![composer_model()],
        }),
    );
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "system",
                json!({"run_id": "run_1", "agent_id": "agent_1"}),
                Some("o1"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "done",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );

    let (_, result) = invoke(&client_for(&bridge), request_with_session(&dir), &dir).await;
    assert_eq!(result.outcome, Outcome::Ok);

    let (store, opened) = SessionStore::open(&dir, "pkt-9:1").unwrap();
    assert_eq!(opened, Opened::Replay);
    assert_eq!(store.stored_result().unwrap().outcome, Outcome::Ok);
    let _ = std::fs::remove_dir_all(&dir);
}
