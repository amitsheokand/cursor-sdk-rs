//! An in-process stand-in for `cursor-sdk-bridge`.
//!
//! It speaks the same Connect-over-HTTP/1.1 protocol the real bridge does, so
//! the tests exercise the actual transport, framing, and error decoding rather
//! than mocks of them. Responses are scripted per test.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cursor_sdk::proto;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// What the fake bridge should do for one RPC.
#[derive(Clone)]
pub enum Reply {
    /// A successful unary response, already encoded.
    Unary(Bytes),
    /// A Connect error: HTTP status, code, message, optional structured detail.
    Error {
        status: StatusCode,
        code: &'static str,
        message: String,
        details: Option<proto::SdkErrorDetails>,
    },
    /// A server stream: pre-framed payloads, then an end-of-stream frame.
    Stream(Vec<Bytes>),
}

impl Reply {
    pub fn unary<M: Message>(message: &M) -> Self {
        Reply::Unary(Bytes::from(message.encode_to_vec()))
    }

    pub fn error(status: StatusCode, code: &'static str, message: &str) -> Self {
        Reply::Error {
            status,
            code,
            message: message.to_string(),
            details: None,
        }
    }

    pub fn error_with_details(
        status: StatusCode,
        code: &'static str,
        details: proto::SdkErrorDetails,
    ) -> Self {
        Reply::Error {
            status,
            code,
            message: "generic transport message".to_string(),
            details: Some(details),
        }
    }
}

#[derive(Default)]
pub struct Recorded {
    /// Every `Service/Method` the client called, in order.
    pub calls: Vec<String>,
    /// The raw request body of each call, keyed by `Service/Method`.
    pub bodies: HashMap<String, Vec<Bytes>>,
}

pub struct FakeBridge {
    pub url: String,
    pub token: String,
    replies: Arc<Mutex<HashMap<String, Vec<Reply>>>>,
    pub recorded: Arc<Mutex<Recorded>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl FakeBridge {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind a loopback port");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let token = "test-bridge-token".to_string();

        let replies: Arc<Mutex<HashMap<String, Vec<Reply>>>> = Arc::new(Mutex::new(HashMap::new()));
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let (shutdown, mut signal) = oneshot::channel();

        let state = (replies.clone(), recorded.clone(), token.clone());
        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = &mut signal => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(_) => continue,
                    },
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, std::convert::Infallible>(handle(state, request).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        // Always answer the handshake checks the client makes on connect.
        let bridge = Self {
            url,
            token,
            replies,
            recorded,
            shutdown: Some(shutdown),
        };
        bridge.always(
            "SdkBridgeControlService/Ping",
            Reply::unary(&proto::PingResponse {
                message: "pong".into(),
            }),
        );
        bridge.always(
            "SdkBridgeControlService/GetVersion",
            Reply::unary(&proto::GetVersionResponse {
                bridge_version: "1.0.0-test".into(),
                protocol_version: "sdk.v1".into(),
                capabilities: vec!["agent.create".into(), "agent.usage".into()],
            }),
        );
        bridge
    }

    /// Queue one reply for an RPC. Queued replies are consumed in order.
    pub fn expect(&self, rpc: &str, reply: Reply) {
        self.replies
            .lock()
            .unwrap()
            .entry(rpc.to_string())
            .or_default()
            .push(reply);
    }

    /// Set the reply used whenever the queue for an RPC is empty.
    pub fn always(&self, rpc: &str, reply: Reply) {
        self.replies
            .lock()
            .unwrap()
            .entry(format!("{rpc}#default"))
            .or_default()
            .push(reply);
    }

    pub fn calls(&self) -> Vec<String> {
        self.recorded.lock().unwrap().calls.clone()
    }

    pub fn call_count(&self, rpc: &str) -> usize {
        self.calls().iter().filter(|call| *call == rpc).count()
    }

    /// Decode the first recorded request body for an RPC.
    pub fn request<M: Message + Default>(&self, rpc: &str) -> M {
        let recorded = self.recorded.lock().unwrap();
        let bodies = recorded
            .bodies
            .get(rpc)
            .unwrap_or_else(|| panic!("{rpc} was never called; calls: {:?}", recorded.calls));
        M::decode(&bodies[0][..]).expect("the request decodes")
    }

    /// Decode the first recorded *streaming* request body, skipping its frame
    /// header.
    pub fn stream_request<M: Message + Default>(&self, rpc: &str) -> M {
        let recorded = self.recorded.lock().unwrap();
        let bodies = recorded
            .bodies
            .get(rpc)
            .unwrap_or_else(|| panic!("{rpc} was never called; calls: {:?}", recorded.calls));
        M::decode(&bodies[0][5..]).expect("the framed request decodes")
    }
}

impl Drop for FakeBridge {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

type State = (
    Arc<Mutex<HashMap<String, Vec<Reply>>>>,
    Arc<Mutex<Recorded>>,
    String,
);

async fn handle(state: State, request: Request<Incoming>) -> Response<Full<Bytes>> {
    let (replies, recorded, token) = state;
    let path = request
        .uri()
        .path()
        .trim_start_matches("/sdk.v1.")
        .to_string();

    let presented = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // The real bridge rejects a missing or wrong token on every RPC, streams
    // included.
    if presented != format!("Bearer {token}") {
        return connect_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "Unauthorized",
            None,
        );
    }

    let body = request.into_body().collect().await.unwrap().to_bytes();
    {
        let mut recorded = recorded.lock().unwrap();
        recorded.calls.push(path.clone());
        recorded.bodies.entry(path.clone()).or_default().push(body);
    }

    let reply = {
        let mut replies = replies.lock().unwrap();
        let queued = replies
            .get_mut(&path)
            .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)));
        queued.or_else(|| {
            replies
                .get(&format!("{path}#default"))
                .and_then(|queue| queue.first().cloned())
        })
    };

    match reply {
        Some(Reply::Unary(payload)) => Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, "application/proto")
            .body(Full::new(payload))
            .unwrap(),
        Some(Reply::Error {
            status,
            code,
            message,
            details,
        }) => connect_error(status, code, &message, details),
        Some(Reply::Stream(frames)) => {
            let mut body = Vec::new();
            for frame in frames {
                body.extend_from_slice(&frame);
            }
            body.extend_from_slice(&end_of_stream(None));
            Response::builder()
                .status(StatusCode::OK)
                .header(hyper::header::CONTENT_TYPE, "application/connect+proto")
                .body(Full::new(Bytes::from(body)))
                .unwrap()
        }
        None => connect_error(
            StatusCode::NOT_IMPLEMENTED,
            "unimplemented",
            &format!("the fake bridge has no reply scripted for {path}"),
            None,
        ),
    }
}

fn connect_error(
    status: StatusCode,
    code: &str,
    message: &str,
    details: Option<proto::SdkErrorDetails>,
) -> Response<Full<Bytes>> {
    use base64::Engine as _;
    let mut body = serde_json::json!({"code": code, "message": message});
    if let Some(details) = details {
        body["details"] = serde_json::json!([{
            "type": "type.googleapis.com/sdk.v1.SdkErrorDetails",
            "value": base64::engine::general_purpose::STANDARD_NO_PAD
                .encode(details.encode_to_vec()),
        }]);
    }
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

/// Frame one protobuf message the way the Connect streaming protocol does.
pub fn frame<M: Message>(message: &M) -> Bytes {
    frame_bytes(0, &message.encode_to_vec())
}

pub fn frame_bytes(flags: u8, payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

/// The end-of-stream frame, optionally carrying an error.
pub fn end_of_stream(error: Option<serde_json::Value>) -> Bytes {
    let body = match error {
        Some(error) => serde_json::json!({"error": error}).to_string(),
        None => "{}".to_string(),
    };
    frame_bytes(0x02, body.as_bytes())
}

/// A `RunStreamMessage` carrying only an envelope case this crate does not
/// know, to prove unknown cases are skipped rather than fatal.
///
/// Hand-encoded: field 7 does not exist in the current contract.
pub fn unknown_envelope_frame() -> Bytes {
    // tag 7, wire type 2 (length-delimited), with a one-byte body.
    let payload = vec![(7 << 3) | 2, 1, 0x00];
    frame_bytes(0, &payload)
}

/// A keepalive: no envelope case and no offset.
pub fn keepalive_frame() -> Bytes {
    frame(&proto::RunStreamMessage::default())
}

pub fn sdk_message_frame(kind: &str, payload: serde_json::Value, offset: Option<&str>) -> Bytes {
    frame(&proto::RunStreamMessage {
        offset: offset.map(str::to_string),
        envelope: Some(proto::run_stream_message::Envelope::SdkMessage(
            proto::SdkMessage {
                r#type: kind.to_string(),
                message: cursor_sdk::json::json_to_struct(&payload),
            },
        )),
    })
}

pub fn result_frame(
    agent_id: &str,
    run_id: &str,
    status: proto::RunLifecycleStatus,
    text: &str,
) -> Bytes {
    frame(&proto::RunStreamMessage {
        offset: None,
        envelope: Some(proto::run_stream_message::Envelope::Result(
            proto::RunStreamResult {
                agent_id: agent_id.to_string(),
                run_id: run_id.to_string(),
                status: status as i32,
                error_code: None,
                result: Some(proto::RunResult {
                    run_id: run_id.to_string(),
                    agent_id: agent_id.to_string(),
                    status: status as i32,
                    result: text.to_string(),
                    duration_ms: 1234,
                    ..Default::default()
                }),
            },
        )),
    })
}

pub fn done_frame(agent_id: &str, run_id: &str) -> Bytes {
    frame(&proto::RunStreamMessage {
        offset: None,
        envelope: Some(proto::run_stream_message::Envelope::Done(
            proto::RunStreamDone {
                agent_id: agent_id.to_string(),
                run_id: run_id.to_string(),
            },
        )),
    })
}

/// A client attached to this fake bridge.
pub fn client_for(bridge: &FakeBridge) -> cursor_sdk::Client {
    cursor_sdk::Client::builder()
        .api_key("test-api-key")
        .endpoint(&bridge.url, &bridge.token)
        .build()
}
