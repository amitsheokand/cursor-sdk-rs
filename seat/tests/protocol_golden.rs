//! Golden JSON tests pinning seat protocol v1.
//!
//! If any of these fail, the protocol drifted: update PROTOCOL.md and bump
//! `PROTOCOL_V` deliberately, never silently.

mod support;

use cursor_seat::{
    ContextChange, ContextChangeKind, ControlInput, ControlMode, Limits, ModelParams, ModelRef,
    Outcome, PromptPart, SeatEvent, SeatEventKind, SeatRequest, SeatResult, SeatUsage,
};
use serde_json::json;

fn minimal_request() -> SeatRequest {
    SeatRequest {
        v: 1,
        request_id: "pkt-1:1".into(),
        cwd: "/repo".into(),
        model: ModelRef {
            id: "composer-2.5".into(),
            params: ModelParams {
                effort: Some("high".into()),
                extra: Default::default(),
            },
        },
        prompt: PromptPart {
            task: "task".into(),
            effort_tag: "high".into(),
            body: "do the thing".into(),
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
        session_dir: Some("seats/pkt-1:1".into()),
        fence: vec![],
        protected_roots: vec![],
    }
}

#[test]
fn seat_request_round_trips_with_defaults() {
    // clip_chars defaults to 40000 when omitted.
    let raw = json!({
        "v": 1,
        "request_id": "pkt-1:1",
        "cwd": "/repo",
        "model": {"id": "composer-2.5", "params": {"effort": "high"}},
        "prompt": {"task": "t", "effort_tag": "high", "body": "b"},
        "disallowed_tools": ["task"],
        "limits": {"context_chars": 100000, "timeout_s": 600, "heartbeat_s": 30},
    });
    let mut req: SeatRequest = serde_json::from_value(raw).expect("parses");
    assert_eq!(req.limits.clip_chars, 40_000);
    let dir = support::workspace_dir();
    req.cwd = dir.to_string_lossy().into_owned();
    assert!(req.validate().is_ok());

    let again: SeatRequest =
        serde_json::from_str(&serde_json::to_string(&minimal_request()).unwrap()).unwrap();
    assert_eq!(again, minimal_request());
}

#[test]
fn seat_request_rejects_unknown_fields() {
    let mut raw = serde_json::to_value(minimal_request()).unwrap();
    raw["bogus"] = json!(1);
    assert!(serde_json::from_value::<SeatRequest>(raw).is_err());

    // fast is not expressible: fixed non-fast law, so the key is unknown.
    let mut params = serde_json::to_value(minimal_request()).unwrap();
    params["model"]["params"]["fast"] = json!(false);
    assert!(serde_json::from_value::<SeatRequest>(params).is_err());

    let mut req = minimal_request();
    let dir = support::workspace_dir();
    req.cwd = dir.to_string_lossy().into_owned();
    req.v = 999;
    assert!(req.validate().is_err());
}

#[test]
fn control_input_rejects_unknown_fields() {
    let raw = json!({
        "id": "c1", "kind": "control", "mode": "hard", "reason": "stop",
    });
    let input: ControlInput = serde_json::from_value(raw).unwrap();
    assert_eq!(input.id, "c1");

    let bad = json!({
        "id": "c2", "kind": "control", "mode": "hard", "bogus": 1,
    });
    assert!(serde_json::from_value::<ControlInput>(bad).is_err());
}

#[test]
fn seat_events_carry_seq_and_snake_case_type() {
    let event = SeatEvent {
        seq: 7,
        kind: SeatEventKind::ToolCall {
            label: "jev/jev_verify".into(),
            status: "completed".into(),
            call_id: "call_1".into(),
        },
    };
    let raw = serde_json::to_value(&event).unwrap();
    assert_eq!(raw["seq"], json!(7));
    assert_eq!(raw["type"], json!("tool_call"));
    let back: SeatEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(back, event);

    let started = SeatEvent {
        seq: 0,
        kind: SeatEventKind::SeatStarted {
            request_id: "pkt-1:1".into(),
        },
    };
    let raw = serde_json::to_value(&started).unwrap();
    assert_eq!(raw["type"], json!("seat_started"));
}

#[test]
fn control_modes_and_settings_shape_are_pinned() {
    for (mode, wire) in [
        (ControlMode::Hard, "hard"),
        (ControlMode::WhenIdle, "when_idle"),
        (ControlMode::Heartbeat, "heartbeat"),
        (ControlMode::Settings, "settings"),
    ] {
        let raw = serde_json::json!({
            "id": "c1", "kind": "control", "mode": wire, "reason": "r",
        });
        let input: ControlInput = serde_json::from_value(raw).unwrap();
        assert_eq!(input.mode, mode);
    }

    // settings parameters shape: exactly {reasoning_effort}.
    let raw = serde_json::json!({
        "id": "c2", "kind": "control", "mode": "settings",
        "parameters": {"reasoning_effort": "xhigh"},
    });
    let input: ControlInput = serde_json::from_value(raw).unwrap();
    assert_eq!(
        input
            .parameters
            .as_ref()
            .and_then(|p| p.reasoning_effort.as_deref()),
        Some("xhigh")
    );

    // Serialized non-settings inputs omit `parameters` (PROTOCOL.md).
    let hard = ControlInput {
        id: "c3".into(),
        kind: cursor_seat::ControlKind::Control,
        mode: ControlMode::Hard,
        reason: "r".into(),
        parameters: None,
    };
    let raw = serde_json::to_value(&hard).unwrap();
    assert!(!raw.as_object().unwrap().contains_key("parameters"));
}

#[test]
fn jev_event_serialization_is_pinned() {
    let event = SeatEvent {
        seq: 8,
        kind: SeatEventKind::Jev {
            check: "receipt_supported".into(),
            verdict: "verified".into(),
            p: 0.93,
        },
    };
    let raw = serde_json::to_value(&event).unwrap();
    assert_eq!(raw["type"], serde_json::json!("jev"));
    assert_eq!(raw["check"], serde_json::json!("receipt_supported"));
    let back: SeatEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(back, event);
}

#[test]
fn result_event_covers_the_outcome_table() {
    for outcome in [
        Outcome::Ok,
        Outcome::Failed,
        Outcome::StartupError,
        Outcome::Busy,
        Outcome::Bounced,
        Outcome::Stale,
    ] {
        let result = SeatResult {
            outcome,
            status: "finished".into(),
            error_kind: None,
            retryable: false,
            retry_after_ms: None,
            request_id: "pkt-1:1".into(),
            run_id: Some("run_1".into()),
            agent_id: Some("agent_1".into()),
            model: Some("composer-2.5".into()),
            text: "done".into(),
            archive_path: None,
            wall_ms: 123,
            ttfe_ms: Some(45),
            usage: Some(SeatUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                total_tokens: Some(120),
                ..Default::default()
            }),
            attempts: 1,
            self_check: None,
            context_changes: vec![ContextChange {
                kind: ContextChangeKind::Truncated,
                source: "prompt.body".into(),
                reason: "over context_chars".into(),
            }],
            resumed: false,
        };
        let event = SeatEvent {
            seq: 9,
            kind: SeatEventKind::Result(result.clone()),
        };
        let raw = serde_json::to_value(&event).unwrap();
        assert_eq!(raw["type"], json!("result"));
        let back: SeatEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(back, event);
    }
}
