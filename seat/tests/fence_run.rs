//! Fence guard: tool escape, protected roots, git drift.

mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use cursor_seat::inbox::Inbox;
use cursor_seat::protocol::{
    Limits, ModelParams, ModelRef, Outcome, PromptPart, SeatEventKind, SeatRequest, SeatResult,
};
use cursor_seat::run::run_seat;
use cursor_sdk::proto;
use serde_json::json;
use support::*;
use tokio::sync::mpsc;

fn fenced_request(cwd: PathBuf, fence: Vec<String>, protected_roots: Vec<String>) -> SeatRequest {
    SeatRequest {
        v: 1,
        request_id: "pkt-fence:1".into(),
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
        limits: Limits {
            context_chars: 100_000,
            clip_chars: 40_000,
            timeout_s: 600,
            heartbeat_s: 30,
        },
        session_dir: None,
        fence,
        protected_roots,
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

async fn collect(
    bridge: &FakeBridge,
    request: SeatRequest,
) -> (Vec<cursor_seat::protocol::SeatEvent>, SeatResult) {
    let client = client_for(bridge);
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
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
                None => break,
            },
        }
    }
    let returned = returned.expect("run_seat returned");
    (events, result.unwrap_or(returned))
}

fn init_git_repo(dir: &PathBuf) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "seat@test"])
        .current_dir(dir)
        .output()
        .expect("git config email");
    Command::new("git")
        .args(["config", "user.name", "seat"])
        .current_dir(dir)
        .output()
        .expect("git config name");
}

#[tokio::test]
async fn edit_outside_cwd_bounces_and_cancels() {
    let cwd = workspace_dir();
    let outside = workspace_dir();
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
                    "name": "edit",
                    "args": {"path": outside.join("evil.txt").display().to_string()},
                    "status": "started",
                    "call_id": "c1",
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Cancelled,
                "",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );

    let req = fenced_request(cwd, vec!["src".into()], vec![]);
    let (events, result) = collect(&bridge, req).await;

    assert_eq!(bridge.call_count("SdkAgentService/CancelRun"), 1);
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.status, "fence_escape");
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, SeatEventKind::Fence { kind, .. } if kind == "escape"))
    );
}

#[tokio::test]
async fn shell_command_into_protected_root_bounces_and_cancels() {
    let cwd = workspace_dir();
    let protected = workspace_dir();
    fs::create_dir_all(protected.join("crates")).unwrap();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    let target = protected.join("crates/evil.rs");
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
                    "name": "shell",
                    "args": {
                        "cwd": cwd.display().to_string(),
                        "command": format!("cp /dev/null {}", target.display()),
                    },
                    "status": "started",
                    "call_id": "c1",
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Cancelled,
                "",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );

    let req = fenced_request(
        cwd,
        vec!["crates".into()],
        vec![protected.to_string_lossy().into_owned()],
    );
    let (_, result) = collect(&bridge, req).await;

    assert_eq!(bridge.call_count("SdkAgentService/CancelRun"), 1);
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
}

#[tokio::test]
async fn shell_cwd_outside_bounces_and_cancels() {
    let cwd = workspace_dir();
    let outside = workspace_dir();
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
                    "name": "shell",
                    "args": {"cwd": outside.display().to_string(), "command": "true"},
                    "status": "started",
                    "call_id": "c1",
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Cancelled,
                "",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );

    let req = fenced_request(cwd, vec!["src".into()], vec![]);
    let (_, result) = collect(&bridge, req).await;

    assert_eq!(bridge.call_count("SdkAgentService/CancelRun"), 1);
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
}

#[tokio::test]
async fn read_outside_cwd_is_allowed() {
    let cwd = workspace_dir();
    let outside = workspace_dir();
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
                    "args": {"path": outside.join("file.txt").display().to_string()},
                    "status": "completed",
                    "call_id": "c1",
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

    let req = fenced_request(cwd, vec!["src".into()], vec![]);
    let (_, result) = collect(&bridge, req).await;

    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(bridge.call_count("SdkAgentService/CancelRun"), 0);
}

#[tokio::test]
async fn protected_root_midrun_escape_after_tool_call() {
    let cwd = workspace_dir();
    let protected = workspace_dir();
    let tracked = protected.join("mirror.txt");
    fs::write(&tracked, b"before").unwrap();

    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    let mut stream_frames = vec![
        sdk_message_frame(
            "system",
            json!({"run_id": "run_1", "agent_id": "agent_1"}),
            Some("o1"),
        ),
        sdk_message_frame(
            "tool_call",
            json!({
                "name": "read",
                "args": {"path": cwd.join("in.txt").display().to_string()},
                "status": "completed",
                "call_id": "c1",
            }),
            Some("o2"),
        ),
    ];
    stream_frames.extend((0..64).map(|_| keepalive_frame()));
    stream_frames.push(
        result_frame(
            "agent_1",
            "run_1",
            proto::RunLifecycleStatus::Cancelled,
            "",
        ),
    );
    stream_frames.push(done_frame("agent_1", "run_1"));
    bridge.expect("SdkAgentService/Send", Reply::Stream(stream_frames));
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );

    let mut req = fenced_request(
        cwd.clone(),
        vec!["mirror.txt".into()],
        vec![protected.to_string_lossy().into_owned()],
    );
    req.limits.heartbeat_s = 1;
    fs::write(cwd.join("in.txt"), b"ok").unwrap();

    let tracked_mut = tracked.clone();
    let client = client_for(&bridge);
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(&client, req, inbox, tx, None);
    tokio::pin!(run_fut);
    let mut result = None;
    let mut saw_tool = false;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if result.is_none() => {
                result = Some(outcome);
            }
            event = rx.recv() => match event {
                Some(event) => {
                    if matches!(&event.kind, SeatEventKind::ToolCall { .. }) {
                        saw_tool = true;
                        fs::write(&tracked_mut, b"after").unwrap();
                    }
                    if let SeatEventKind::Result(r) = event.kind {
                        result = Some(r);
                    }
                }
                None => break,
            },
        }
        if result.is_some() {
            break;
        }
    }
    let result = result.expect("result");
    assert!(saw_tool);
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
    assert_eq!(bridge.call_count("SdkAgentService/CancelRun"), 1);
}

#[tokio::test]
async fn protected_root_change_bounces() {
    let cwd = workspace_dir();
    let protected = workspace_dir();
    let tracked = protected.join("mirror.txt");
    fs::write(&tracked, b"before").unwrap();

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
                "assistant",
                json!({"message": {"content": [{"type": "text", "text": "working"}]}}),
                Some("o2"),
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

    let req = fenced_request(
        cwd,
        vec!["mirror.txt".into()],
        vec![protected.to_string_lossy().into_owned()],
    );
    let tracked_mut = tracked.clone();
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
            event = rx.recv() => match event {
                Some(event) => {
                    if matches!(&event.kind, SeatEventKind::RunStarted { .. }) {
                        fs::write(&tracked_mut, b"after").unwrap();
                    }
                    if let SeatEventKind::Result(r) = event.kind {
                        result = Some(r);
                    }
                }
                None => break,
            }
        }
        if result.is_some() {
            break;
        }
    }
    let result = result.expect("result");
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
}

#[tokio::test]
async fn out_of_fence_git_drift_gets_one_correction_then_fails() {
    let cwd = workspace_dir();
    init_git_repo(&cwd);
    fs::write(cwd.join("in_fence.txt"), b"ok").unwrap();
    fs::write(cwd.join("outside.txt"), b"bad").unwrap();
    Command::new("git")
        .args(["add", "in_fence.txt"])
        .current_dir(&cwd)
        .output()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&cwd)
        .output()
        .expect("git commit");

    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    let happy_end = vec![
        result_frame(
            "agent_1",
            "run_1",
            proto::RunLifecycleStatus::Finished,
            "done",
        ),
        done_frame("agent_1", "run_1"),
    ];
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(
            vec![sdk_message_frame(
                "system",
                json!({"run_id": "run_1", "agent_id": "agent_1"}),
                Some("o1"),
            )]
            .into_iter()
            .chain(happy_end.clone())
            .collect(),
        ),
    );
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(
            vec![sdk_message_frame(
                "system",
                json!({"run_id": "run_2", "agent_id": "agent_1"}),
                Some("o1"),
            )]
            .into_iter()
            .chain(happy_end)
            .collect(),
        ),
    );

    let req = fenced_request(cwd, vec!["in_fence.txt".into()], vec![]);
    let (events, result) = collect(&bridge, req).await;

    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SeatEventKind::Fence { kind, path, .. }
                if kind == "drift" && path == "outside.txt"
        ))
    );
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 2);
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.status, "fence_drift");
    assert_eq!(result.error_kind.as_deref(), Some("FenceDrift"));
}

#[tokio::test]
async fn in_fence_git_change_stays_ok() {
    let cwd = workspace_dir();
    init_git_repo(&cwd);
    fs::write(cwd.join("allowed.txt"), b"v1").unwrap();
    Command::new("git")
        .args(["add", "allowed.txt"])
        .current_dir(&cwd)
        .output()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&cwd)
        .output()
        .expect("git commit");
    fs::write(cwd.join("allowed.txt"), b"v2").unwrap();

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
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Finished,
                "done",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );

    let req = fenced_request(cwd, vec!["allowed.txt".into()], vec![]);
    let (_, result) = collect(&bridge, req).await;

    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 1);
}

#[tokio::test]
async fn dotdot_edit_outside_newfile_bounces_and_cancels() {
    let cwd = workspace_dir();
    let outside = workspace_dir();
    let bridge = FakeBridge::start().await;
    script_models_create_close(&bridge);
    let escape_path = format!("../{}/evil.txt", outside.file_name().unwrap().to_string_lossy());
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
                    "name": "edit",
                    "args": {"path": escape_path},
                    "status": "started",
                    "call_id": "c1",
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Cancelled,
                "",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );
    let (_, result) = collect(&bridge, fenced_request(cwd, vec!["src".into()], vec![])).await;
    assert_eq!(result.outcome, Outcome::Bounced);
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
}

#[cfg(unix)]
#[tokio::test]
async fn edit_under_symlink_outside_bounces() {
    use std::os::unix::fs::symlink;
    let cwd = workspace_dir();
    let outside = workspace_dir();
    symlink(&outside, cwd.join("link")).unwrap();
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
                    "name": "edit",
                    "args": {"path": "link/new.txt"},
                    "status": "started",
                    "call_id": "c1",
                }),
                Some("o2"),
            ),
            result_frame(
                "agent_1",
                "run_1",
                proto::RunLifecycleStatus::Cancelled,
                "",
            ),
            done_frame("agent_1", "run_1"),
        ]),
    );
    bridge.always(
        "SdkAgentService/CancelRun",
        Reply::unary(&proto::CancelRunResponse {}),
    );
    let (_, result) = collect(&bridge, fenced_request(cwd, vec!["src".into()], vec![])).await;
    assert_eq!(result.error_kind.as_deref(), Some("FenceEscape"));
}

#[tokio::test]
async fn attach_resume_drift_correction_then_fence_drift() {
    use cursor_seat::session::{Opened, OpState, SessionStore};

    let cwd = workspace_dir();
    init_git_repo(&cwd);
    fs::write(cwd.join("in_fence.txt"), b"ok").unwrap();
    Command::new("git")
        .args(["add", "in_fence.txt"])
        .current_dir(&cwd)
        .output()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&cwd)
        .output()
        .expect("git commit");
    fs::write(cwd.join("outside.txt"), b"bad").unwrap();

    let session_dir = workspace_dir();
    let (mut store, opened) = SessionStore::open(&session_dir, "pkt-resume:1").unwrap();
    assert_eq!(opened, Opened::Fresh);
    store.set_state(OpState::Awaiting).unwrap();
    store.record_run("run_r", "agent_r").unwrap();
    drop(store);

    let bridge = FakeBridge::start().await;
    let happy = vec![
        result_frame(
            "agent_r",
            "run_r",
            proto::RunLifecycleStatus::Finished,
            "done",
        ),
        done_frame("agent_r", "run_r"),
    ];
    bridge.expect(
        "SdkAgentService/ObserveRun",
        Reply::Stream(
            vec![sdk_message_frame(
                "system",
                json!({"run_id": "run_r", "agent_id": "agent_r"}),
                Some("o1"),
            )]
            .into_iter()
            .chain(happy.clone())
            .collect(),
        ),
    );
    bridge.expect(
        "SdkAgentService/Send",
        Reply::Stream(
            vec![sdk_message_frame(
                "system",
                json!({"run_id": "run_r2", "agent_id": "agent_r"}),
                Some("o1"),
            )]
            .into_iter()
            .chain(happy)
            .collect(),
        ),
    );

    let mut req = fenced_request(cwd, vec!["in_fence.txt".into()], vec![]);
    req.request_id = "pkt-resume:1".into();
    req.session_dir = Some(session_dir.to_string_lossy().into_owned());

    let client = client_for(&bridge);
    let (store, opened) = SessionStore::open(&session_dir, "pkt-resume:1").unwrap();
    assert!(matches!(opened, Opened::Resume { .. }));
    let inbox = Inbox::new(&[]).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run_fut = run_seat(&client, req, inbox, tx, Some(store));
    tokio::pin!(run_fut);
    let mut returned = None;
    let mut result = None;
    loop {
        tokio::select! {
            outcome = &mut run_fut, if returned.is_none() => {
                returned = Some(outcome);
            }
            event = rx.recv() => match event {
                Some(event) => {
                    if let SeatEventKind::Result(r) = event.kind {
                        result = Some(r);
                    }
                }
                None => break,
            },
        }
    }
    let result = result.unwrap_or_else(|| returned.expect("run_seat returned"));
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 1);
    assert_eq!(result.outcome, Outcome::Failed);
    assert_eq!(result.status, "fence_drift");
}

#[test]
fn request_without_fence_fields_round_trips() {
    let raw = json!({
        "v": 1,
        "request_id": "pkt-1:1",
        "cwd": "/unused",
        "model": {"id": "composer-2.5", "params": {"effort": "high"}},
        "prompt": {"task": "t", "effort_tag": "high", "body": "b"},
        "disallowed_tools": ["task"],
        "limits": {"context_chars": 100000, "timeout_s": 600, "heartbeat_s": 30},
    });
    let mut req: SeatRequest = serde_json::from_value(raw).expect("parses");
    assert!(req.fence.is_empty());
    assert!(req.protected_roots.is_empty());
    let dir = workspace_dir();
    req.cwd = dir.to_string_lossy().into_owned();
    assert!(req.validate().is_ok());
}
