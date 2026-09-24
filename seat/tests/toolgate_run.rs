//! Toolgate config, registry declarations, fence, and tool_stats.

mod support;

use cursor_sdk::proto;
use cursor_seat::inbox::Inbox;
use cursor_seat::protocol::{
    Limits, ModelParams, ModelRef, Outcome, PromptPart, SeatEventKind, SeatRequest, ToolgateConfig,
    ToolgateMode,
};
use cursor_seat::run::run_seat;
use cursor_seat::toolgate::{
    invoke_tool_blocking, ToolgateContext, TOOL_EDIT_DIFF, TOOL_READ_WINDOW, TOOL_RUN_BOUNDED,
};
use hyper::StatusCode;
use serde_json::json;
use std::fs;
use support::*;
use tokio::sync::mpsc;

fn base_request(cwd: std::path::PathBuf, toolgate: ToolgateConfig) -> SeatRequest {
    SeatRequest {
        v: 1,
        request_id: "pkt-tg:1".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: ModelRef {
            id: "composer-2.5".into(),
            params: ModelParams {
                effort: Some("high".into()),
                extra: Default::default(),
            },
        },
        prompt: PromptPart {
            task: "t".into(),
            effort_tag: "high".into(),
            body: "body".into(),
            steer: None,
        },
        mcp_servers: vec![],
        disallowed_tools: vec!["task".into()],
        tools_enabled: vec![],
        skill_roots: vec![],
        jev: Default::default(),
        toolgate,
        limits: Limits {
            context_chars: 100_000,
            clip_chars: 40_000,
            timeout_s: 600,
            heartbeat_s: 30,
        },
        session_dir: None,
        fence: vec![],
        protected_roots: vec![],
    }
}

fn script_models_create_close(bridge: &FakeBridge) {
    bridge.expect(
        "SdkCursorService/ListModels",
        Reply::unary(&proto::ListModelsResponse {
            items: vec![proto::SdkModel {
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
            }],
        }),
    );
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
}

async fn collect(bridge: &FakeBridge, request: SeatRequest) {
    let client = client_for(bridge);
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(&client, request, inbox, tx, None);
    tokio::pin!(run_fut);
    let mut returned = None;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if returned.is_none() => {
                returned = Some(outcome);
            }
            _ = rx.recv() => {}
        }
        if returned.is_some() {
            break;
        }
    }
}

#[tokio::test]
async fn off_registers_no_toolgate_tools() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    let req = base_request(cwd, ToolgateConfig::default());
    collect(&bridge, req).await;
    let create: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    let keys: Vec<String> = create
        .options
        .unwrap()
        .local
        .unwrap()
        .custom_tools
        .keys()
        .filter(|k| k.contains("window") || k.contains("edit_diff") || k.contains("run_"))
        .cloned()
        .collect();
    assert!(keys.is_empty());
}

#[tokio::test]
async fn replace_disallows_builtin_read_edit_write_shell() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    let req = base_request(
        cwd,
        ToolgateConfig {
            mode: ToolgateMode::Replace,
            gates: vec![],
        },
    );
    collect(&bridge, req).await;
    let create: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    let disallowed = create.options.unwrap().disallowed_tools;
    for name in ["read", "edit", "write", "shell", "task"] {
        assert!(
            disallowed.iter().any(|t| t == name),
            "missing disallowed {name}"
        );
    }
}

#[tokio::test]
async fn add_declares_toolgate_custom_tools() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    let req = base_request(
        cwd,
        ToolgateConfig {
            mode: ToolgateMode::Add,
            gates: vec![],
        },
    );
    collect(&bridge, req).await;
    let create: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    let tools = create.options.unwrap().local.unwrap().custom_tools;
    assert!(tools.contains_key("read_window"));
    assert!(tools.contains_key("edit_diff"));
    assert!(tools.contains_key("run_bounded"));
}

#[tokio::test]
async fn read_window_round_trips() {
    let cwd = workspace_dir();
    fs::write(cwd.join("sample.txt"), "line1\nline2\n").unwrap();
    let ctx = ToolgateContext {
        cwd: cwd.clone(),
        fence: vec![],
        gates: vec![],
        allowed_extra: vec![],
        protected_roots: vec![],
    };
    let out = invoke_tool_blocking(
        &ctx,
        TOOL_READ_WINDOW,
        json!({"path": "sample.txt", "line": 1, "radius": 1}),
    )
    .await;
    assert!(out.get("error").is_none());
    assert_eq!(out["total"], 2);
}

#[tokio::test]
async fn run_bounded_round_trips() {
    let cwd = workspace_dir();
    let ctx = ToolgateContext {
        cwd: cwd.clone(),
        fence: vec![],
        gates: vec![],
        allowed_extra: vec![],
        protected_roots: vec![],
    };
    let out = invoke_tool_blocking(
        &ctx,
        TOOL_RUN_BOUNDED,
        json!({"program": "true", "args": []}),
    )
    .await;
    assert!(out.get("error").is_none());
    assert_eq!(out["code"], 0);
}

#[tokio::test]
async fn tool_stats_count_completed_tool_calls() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "system",
                json!({"run_id": "run_1", "agent_id": "agent_1"}),
                Some("o1"),
            ),
            sdk_message_frame(
                "tool_call",
                json!({
                    "name": "read",
                    "args": {"path": "x"},
                    "status": "completed",
                    "call_id": "c1",
                    "result": {"text": "hello"},
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "ok",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    let client = client_for(&bridge);
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(
        &client,
        base_request(cwd, ToolgateConfig::default()),
        inbox,
        tx,
        None,
    );
    tokio::pin!(run_fut);
    let mut result = None;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if result.is_none() => {
                result = Some(outcome);
            }
            event = rx.recv() => {
                if let Some(event) = event {
                    if let SeatEventKind::Result(r) = event.kind {
                        result = Some(r);
                    }
                } else {
                    break;
                }
            }
        }
        if result.is_some() {
            break;
        }
    }
    let result = result.expect("result");
    let stat = result.tool_stats.get("read").expect("read stats");
    assert_eq!(stat.calls, 1);
    assert_eq!(stat.result_chars, 5);
}

#[tokio::test]
async fn edit_diff_outside_fence_is_rejected() {
    let cwd = workspace_dir();
    fs::write(cwd.join("allowed.txt"), b"a").unwrap();
    fs::write(cwd.join("outside.txt"), b"a").unwrap();
    let ctx = ToolgateContext {
        cwd: cwd.clone(),
        fence: vec!["allowed.txt".into()],
        gates: vec![],
        allowed_extra: vec![],
        protected_roots: vec![],
    };
    let out = invoke_tool_blocking(
        &ctx,
        TOOL_EDIT_DIFF,
        json!({"path": "outside.txt", "old": "a", "new": "b"}),
    )
    .await;
    assert!(out.get("error").is_some());
    let err = out["error"].as_str().unwrap_or_default();
    assert!(err.contains("fence"));
}

#[tokio::test]
async fn run_bounded_into_protected_root_returns_error_before_run() {
    let cwd = workspace_dir();
    let protected = workspace_dir();
    fs::create_dir_all(protected.join("crates")).unwrap();
    let src = cwd.join("a.rs");
    fs::write(&src, b"x").unwrap();
    let target = protected.join("crates/evil.rs");
    let ctx = ToolgateContext {
        cwd: cwd.clone(),
        fence: vec![".".into()],
        gates: vec![],
        allowed_extra: vec![],
        protected_roots: vec![protected.clone()],
    };
    let out = invoke_tool_blocking(
        &ctx,
        TOOL_RUN_BOUNDED,
        json!({
            "program": "cp",
            "args": [src.display().to_string(), target.display().to_string()],
        }),
    )
    .await;
    assert!(out.get("error").is_some());
    assert!(!target.exists());
}

fn validation_create_agent_error(message: &str) -> Reply {
    Reply::error_with_details(
        StatusCode::BAD_REQUEST,
        "invalid_argument",
        proto::SdkErrorDetails {
            sdk_error_code: proto::SdkErrorCode::ValidationError as i32,
            message: message.to_string(),
            ..Default::default()
        },
    )
}

fn list_models_reply() -> Reply {
    Reply::unary(&proto::ListModelsResponse {
        items: vec![proto::SdkModel {
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
        }],
    })
}

#[tokio::test]
async fn replace_falls_back_only_when_create_agent_rejects_disallowed_tool() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    bridge.expect("SdkCursorService/ListModels", list_models_reply());
    bridge.expect(
        "SdkAgentService/CreateAgent",
        validation_create_agent_error("unknown tool name: shell"),
    );
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::unary(&proto::CreateAgentResponse {
            agent_id: "agent_1".into(),
            model: None,
        }),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    let mut req = base_request(
        cwd,
        ToolgateConfig {
            mode: ToolgateMode::Replace,
            gates: vec![],
        },
    );
    req.disallowed_tools = vec!["task".into(), "read".into()];
    collect(&bridge, req).await;
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 2);
    let recorded = bridge.recorded.lock().unwrap();
    let bodies = recorded
        .bodies
        .get("SdkAgentService/CreateAgent")
        .expect("create bodies");
    let second: proto::CreateAgentRequest =
        prost::Message::decode(&bodies.last().expect("second create")[..]).unwrap();
    let disallowed = second.options.unwrap().disallowed_tools;
    assert!(
        disallowed.iter().any(|t| t == "read"),
        "pre-existing disallowed read must survive fallback"
    );
    assert!(
        !disallowed.iter().any(|t| t == "shell"),
        "replace-added shell must be reverted after fallback"
    );
}

#[tokio::test]
async fn replace_does_not_fallback_on_unrelated_create_agent_error() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    bridge.expect("SdkCursorService/ListModels", list_models_reply());
    bridge.expect(
        "SdkAgentService/CreateAgent",
        Reply::error_with_details(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            proto::SdkErrorDetails {
                sdk_error_code: proto::SdkErrorCode::InvalidModel as i32,
                message: "model missing".to_string(),
                ..Default::default()
            },
        ),
    );
    let req = base_request(
        cwd,
        ToolgateConfig {
            mode: ToolgateMode::Replace,
            gates: vec![],
        },
    );
    let client = client_for(&bridge);
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(&client, req, inbox, tx, None);
    tokio::pin!(run_fut);
    let mut result = None;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if result.is_none() => {
                result = Some(outcome);
            }
            event = rx.recv() => {
                if let Some(event) = event {
                    if let SeatEventKind::Result(r) = event.kind {
                        result = Some(r);
                    }
                } else {
                    break;
                }
            }
        }
        if result.is_some() {
            break;
        }
    }
    let result = result.expect("result");
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 1);
}

#[tokio::test]
async fn tool_stats_counts_custom_tool_callback_wire_size() {
    let cwd = workspace_dir();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    let payload = json!({"path": "sample.txt", "lines": "1|a"});
    let expected = payload.to_string().chars().count() as u64;
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(vec![
            sdk_message_frame(
                "system",
                json!({"run_id": "run_1", "agent_id": "agent_1"}),
                Some("o1"),
            ),
            sdk_message_frame(
                "tool_call",
                json!({
                    "name": "read_window",
                    "args": {"path": "sample.txt"},
                    "status": "completed",
                    "call_id": "c1",
                    "result": payload,
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "ok",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    let client = client_for(&bridge);
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(
        &client,
        base_request(
            cwd,
            ToolgateConfig {
                mode: ToolgateMode::Add,
                gates: vec![],
            },
        ),
        inbox,
        tx,
        None,
    );
    tokio::pin!(run_fut);
    let mut result = None;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if result.is_none() => {
                result = Some(outcome);
            }
            event = rx.recv() => {
                if let Some(event) = event {
                    if let SeatEventKind::Result(r) = event.kind {
                        result = Some(r);
                    }
                } else {
                    break;
                }
            }
        }
        if result.is_some() {
            break;
        }
    }
    let result = result.expect("result");
    let stat = result
        .tool_stats
        .get("read_window")
        .expect("read_window stats");
    assert_eq!(stat.calls, 1);
    assert_eq!(stat.result_chars, expected);
}

fn ok_stream(text: &str) -> Vec<bytes::Bytes> {
    vec![
        sdk_message_frame(
            "system",
            json!({"run_id": "run_1", "agent_id": "agent_1"}),
            Some("o1"),
        ),
        result_frame(
            "agent_1",
            "run_1",
            proto::RunLifecycleStatus::Finished,
            text,
        ),
        done_frame("agent_1", "run_1"),
    ]
}
