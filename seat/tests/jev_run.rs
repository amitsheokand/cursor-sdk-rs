//! P6 run-level tests: self-check follow-ups and Unknown triage against
//! a local mock System One server (no network). `TYPESAFE_ENDPOINT` and
//! `TYPESAFE_API_KEY` are set for the whole test (single test function,
//! scenarios sequential, env restored afterwards).

mod support;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cursor_sdk::proto;
use cursor_seat::inbox::Inbox;
use cursor_seat::protocol::{
    JevConfig, Limits, ModelParams, ModelRef, Outcome, PromptPart, SeatEvent, SeatEventKind,
    SeatRequest, SeatResult,
};
use cursor_seat::run::run_seat;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use support::*;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Controllable mock: noul probability for `receipt_supported`, plus an
/// auth/model assertion latch.
async fn mock_server(
    p: Arc<Mutex<f64>>,
    saw_auth: Arc<Mutex<bool>>,
    saw_model: Arc<Mutex<bool>>,
) -> String {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (p, saw_auth, saw_model) = (p.clone(), saw_auth.clone(), saw_model.clone());
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let (p, saw_auth, saw_model) = (p.clone(), saw_auth.clone(), saw_model.clone());
                    async move {
                        let auth = request
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        let payload: serde_json::Value =
                            serde_json::from_slice(&body).unwrap_or_default();
                        if auth == "Bearer test-key" {
                            *saw_auth.lock().unwrap() = true;
                        }
                        if payload.get("model").and_then(|m| m.as_str()) == Some("jev-1.13.0") {
                            *saw_model.lock().unwrap() = true;
                        }
                        let mut answers = serde_json::Map::new();
                        if let Some(questions) =
                            payload.get("questions").and_then(|q| q.as_object())
                        {
                            for id in questions.keys() {
                                match id.as_str() {
                                    "receipt_supported" => {
                                        answers.insert(
                                            id.clone(),
                                            json!({"type": "noul", "noul": *p.lock().unwrap()}),
                                        );
                                    }
                                    // Trivial-task gate: confident-trivial
                                    // in the mock; scenarios needing the
                                    // self-check omit trivial.toml.
                                    "trivial_task" => {
                                        answers.insert(
                                            id.clone(),
                                            json!({"type": "noul", "noul": 0.95, "confidence": 0.9}),
                                        );
                                    }
                                    "triage" => {
                                        answers.insert(
                                            id.clone(),
                                            json!({
                                                "type": "choice",
                                                "choice": "flake",
                                                "probabilities": {"flake": 0.9, "code": 0.1},
                                                "confidence": 0.85,
                                            }),
                                        );
                                    }
                                    _ if id.starts_with("tool:") => {
                                        // Prune battery: keep skill_use,
                                        // drop the rest.
                                        let p = if id.ends_with("skill_use") { 0.9 } else { 0.1 };
                                        answers
                                            .insert(id.clone(), json!({"type": "noul", "noul": p}));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(
                                    json!({
                                        "model": "jev-1.13.0",
                                        "answers": answers,
                                        "usage": {"input_tokens": 1, "output_tokens": 1},
                                    })
                                    .to_string(),
                                )))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    url
}

fn questions_dir() -> TestDir {
    let path = std::env::temp_dir().join(format!("seat-jev-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    let dir = TestDir::from_existing(path);
    std::fs::write(
        dir.join("post.toml"),
        "[question.receipt_supported]\n\
         type = \"noul\"\n\
         instructions = \"Does the receipt claimed outcome match evidence in the diff and gate log?\"\n\
         criteria_true = \"Every claimed file, test, and result appears in the attached evidence.\"\n\
         criteria_false = \"A claim is missing from evidence.\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("triage.toml"),
        "[question.triage]\n\
         type = \"choice\"\n\
         instructions = \"What class best explains this failure?\"\n\
         options = [\"code: wrong impl.\", \"flake: transient.\"]\n",
    )
    .unwrap();
    dir
}

fn request_with_jev(questions: &std::path::Path, cwd: &std::path::Path, turns: u32) -> SeatRequest {
    SeatRequest {
        v: 1,
        request_id: "pkt-6:1".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        model: ModelRef {
            id: "composer-2.5".into(),
            params: ModelParams {
                effort: Some("high".into()),
                extra: HashMap::new(),
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
        toolgate: Default::default(),
        jev: JevConfig {
            enabled: true,
            questions_dir: Some(questions.display().to_string()),
            self_check_turns: turns,
            prune_tools: false,
        },
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

fn ok_stream(text: &str) -> Vec<bytes::Bytes> {
    vec![
        sdk_message_frame(
            "system",
            json!({"run_id": "run_1", "agent_id": "agent_1"}),
            Some("o1"),
        ),
        sdk_message_frame(
            "assistant",
            json!({"message": {"content": [{"type": "text", "text": text}]}}),
            Some("o2"),
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

async fn collect(bridge: &FakeBridge, request: SeatRequest) -> (Vec<SeatEvent>, SeatResult) {
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
    (events, result.expect("a result event"))
}

struct EnvGuard {
    key: Option<String>,
    endpoint: Option<String>,
}

impl EnvGuard {
    fn set(url: &str) -> Self {
        let guard = EnvGuard {
            key: std::env::var("TYPESAFE_API_KEY").ok(),
            endpoint: std::env::var("TYPESAFE_ENDPOINT").ok(),
        };
        std::env::set_var("TYPESAFE_API_KEY", "test-key");
        std::env::set_var("TYPESAFE_ENDPOINT", url);
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.key.take() {
            Some(key) => std::env::set_var("TYPESAFE_API_KEY", key),
            None => std::env::remove_var("TYPESAFE_API_KEY"),
        }
        match self.endpoint.take() {
            Some(endpoint) => std::env::set_var("TYPESAFE_ENDPOINT", endpoint),
            None => std::env::remove_var("TYPESAFE_ENDPOINT"),
        }
    }
}

#[tokio::test]
async fn self_check_and_triage_flow() {
    let p = Arc::new(Mutex::new(0.95));
    let saw_auth = Arc::new(Mutex::new(false));
    let saw_model = Arc::new(Mutex::new(false));
    let url = mock_server(p.clone(), saw_auth.clone(), saw_model.clone()).await;
    let _env = EnvGuard::set(&url);
    let dir = questions_dir();
    let ws = workspace_dir();

    // Scenario A: passing self-check, no follow-up.
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
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    let (events, result) = collect(&bridge, request_with_jev(&dir, &ws, 1)).await;
    assert_eq!(result.outcome, Outcome::Ok);
    let check = result.self_check.expect("self_check recorded");
    assert!(check.passed);
    assert_eq!(check.turns, 0);
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 1);
    assert!(*saw_auth.lock().unwrap(), "bearer key sent");
    assert!(*saw_model.lock().unwrap(), "pinned model sent");
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.kind, SeatEventKind::Jev { .. })),
        "no triage on ok"
    );

    // Scenario B: failing self-check sends one same-agent follow-up.
    *p.lock().unwrap() = 0.05;
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
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("first")));
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("second")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    let (_, result) = collect(&bridge, request_with_jev(&dir, &ws, 1)).await;
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(result.text, "second");
    let check = result.self_check.expect("self_check recorded");
    assert!(!check.passed);
    assert_eq!(check.turns, 1);
    assert!(check.reason.unwrap().contains("receipt_supported"));
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 2);
    assert_eq!(bridge.call_count("SdkAgentService/CreateAgent"), 1);
    // The follow-up references the failed check.
    let bodies = bridge.recorded.lock().unwrap();
    let second = &bodies.bodies["SdkAgentService/Send"][1];
    let asked: proto::SendRequest =
        <proto::SendRequest as prost::Message>::decode(&second[5..]).expect("send decodes");
    assert!(
        asked.message.unwrap().text.contains("receipt_supported"),
        "follow-up names the check"
    );

    // Scenario C: Unknown pre-start failure emits a triage jev event.
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
            hyper::StatusCode::NOT_IMPLEMENTED,
            "unknown",
            proto::SdkErrorDetails {
                sdk_error_code: 9999,
                message: "new".into(),
                ..Default::default()
            },
        ),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    let (events, result) = collect(&bridge, request_with_jev(&dir, &ws, 0)).await;
    assert_eq!(result.error_kind.as_deref(), Some("Unknown"));
    let triage = events.iter().find_map(|e| match &e.kind {
        SeatEventKind::Jev { check, verdict, p } => Some((check.clone(), verdict.clone(), *p)),
        _ => None,
    });
    assert_eq!(
        triage,
        Some(("triage".to_string(), "flake".to_string(), 0.9))
    );

    // Scenario D: trivial task skips the self-check loop (one Send).
    let trivial_path =
        std::env::temp_dir().join(format!("seat-jev-trivial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&trivial_path);
    std::fs::create_dir_all(&trivial_path).unwrap();
    let trivial_dir = TestDir::from_existing(trivial_path);
    std::fs::write(
        trivial_dir.join("post.toml"),
        "[question.receipt_supported]\n\
         type = \"noul\"\n\
         instructions = \"Supported?\"\n\
         criteria_true = \"yes\"\n\
         criteria_false = \"no\"\n",
    )
    .unwrap();
    std::fs::write(
        trivial_dir.join("trivial.toml"),
        "[question.trivial_task]\n\
         type = \"noul\"\n\
         instructions = \"Trivial?\"\n\
         criteria_true = \"one-shot\"\n\
         criteria_false = \"needs work\"\n",
    )
    .unwrap();
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
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    let (_events, result) = collect(&bridge, request_with_jev(&trivial_dir, &ws, 1)).await;
    assert_eq!(result.outcome, Outcome::Ok);
    assert_eq!(result.self_check, None);
    assert_eq!(bridge.call_count("SdkAgentService/Send"), 1);

    // Scenario E: prune_tools declares only the kept tool.
    let prune_path = std::env::temp_dir().join(format!("seat-jev-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&prune_path);
    std::fs::create_dir_all(&prune_path).unwrap();
    let prune_dir = TestDir::from_existing(prune_path);
    std::fs::write(
        prune_dir.join("tools.toml"),
        "[question.select_tools]\n\
         type = \"noul\"\n\
         instructions = \"Is `{name}` relevant? {description}\"\n\
         criteria_true = \"needed\"\n\
         criteria_false = \"unneeded\"\n",
    )
    .unwrap();
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
    bridge.expect("SdkAgentService/Send", Reply::Stream(ok_stream("done")));
    bridge.always(
        "SdkAgentService/GetUsage",
        Reply::unary(&proto::GetUsageResponse::default()),
    );
    bridge.always(
        "SdkAgentService/CloseAgent",
        Reply::unary(&proto::CloseAgentResponse {}),
    );
    let mut pruned = request_with_jev(&prune_dir, &ws, 0);
    pruned.jev.prune_tools = true;
    let (events, result) = collect(&bridge, pruned).await;
    assert_eq!(result.outcome, Outcome::Ok);
    let create: proto::CreateAgentRequest = bridge.request("SdkAgentService/CreateAgent");
    let mut declared: Vec<String> = create
        .options
        .unwrap()
        .local
        .unwrap()
        .custom_tools
        .keys()
        .cloned()
        .collect();
    declared.sort();
    assert_eq!(declared, vec!["skill_use".to_string()]);
    let prune_event = events.iter().find_map(|e| match &e.kind {
        SeatEventKind::Jev { check, verdict, .. } if check == "select_tools" => {
            Some(verdict.clone())
        }
        _ => None,
    });
    assert_eq!(prune_event.as_deref(), Some("skill_use"));
}
