//! The loopback Connect server the bridge calls back into.
//!
//! One server hosts both callback services; the bridge is told about them
//! separately (tools can be registered at runtime, stores only at launch), but
//! they share a port and a bearer token.
//!
//! Two details cost real debugging time if missed, and are handled here:
//!
//! * Callback POSTs may arrive with `Transfer-Encoding: chunked` and no
//!   `Content-Length`. Hyper decodes that for us; a hand-rolled server would
//!   see an empty body.
//! * A tool result is a `google.protobuf.Struct`, so it must be a JSON
//!   *object*. Scalars are wrapped as `{"value": …}` on the way out.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use prost::Message as _;
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::store::{AgentStore, StoreRequest};
use super::tools::{ToolCall, ToolRegistry};
use crate::error::Result;
use crate::json::{json_to_object_struct, json_to_struct, struct_to_json};
use crate::proto;

const TOOL_ROUTE: &str = "/sdk.v1.SdkCustomToolCallbackService/CallCustomTool";
const STORE_ROUTE: &str = "/sdk.v1.SdkStoreCallbackService/CallStore";

/// What the callback server can dispatch to.
struct Services {
    token: String,
    tools: ToolRegistry,
    store: Option<Arc<dyn AgentStore>>,
}

/// A running loopback server for the adapter-implemented callback services.
///
/// Dropping it stops the server.
pub struct CallbackServer {
    url: String,
    token: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl std::fmt::Debug for CallbackServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a secret; keep it out of logs.
        formatter
            .debug_struct("CallbackServer")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl CallbackServer {
    /// Bind a loopback port and start serving.
    ///
    /// The token is the one the bridge must present; generate it with
    /// [`random_token`].
    pub(crate) async fn start(
        tools: ToolRegistry,
        store: Option<Arc<dyn AgentStore>>,
        token: String,
    ) -> Result<Self> {
        let listener =
            TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
        let address = listener.local_addr()?;
        let url = format!("http://127.0.0.1:{}", address.port());

        let services = Arc::new(Services {
            token: token.clone(),
            tools,
            store,
        });
        let (shutdown, mut shutdown_signal) = oneshot::channel();

        tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = &mut shutdown_signal => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            tracing::warn!(target: "cursor_sdk::callback", %error, "accept failed");
                            continue;
                        }
                    },
                };
                let services = Arc::clone(&services);
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let services = Arc::clone(&services);
                        async move { Ok::<_, std::convert::Infallible>(serve(services, request).await) }
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        tracing::debug!(target: "cursor_sdk::callback", %error, "connection ended");
                    }
                });
            }
            tracing::debug!(target: "cursor_sdk::callback", "callback server stopped");
        });

        tracing::debug!(target: "cursor_sdk::callback", %url, "callback server listening");
        Ok(Self {
            url,
            token,
            shutdown: Some(shutdown),
        })
    }

    /// The base URL to give the bridge.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The bearer token the bridge must present.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Which body encoding a request used, so the reply matches it.
#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    Proto,
    Json,
}

impl Encoding {
    fn content_type(self) -> &'static str {
        match self {
            Encoding::Proto => "application/proto",
            Encoding::Json => "application/json",
        }
    }
}

async fn serve(services: Arc<Services>, request: Request<Incoming>) -> Response<Full<Bytes>> {
    let path = request.uri().path().to_string();

    if request.method() != hyper::Method::POST {
        return connect_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "unimplemented",
            "callbacks are POSTs",
        );
    }

    // Validate the bridge's bearer token exactly as the bridge validates ours.
    let presented = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), services.token.as_bytes()) {
        tracing::warn!(target: "cursor_sdk::callback", %path, "rejected a callback with a bad token");
        return connect_error(StatusCode::UNAUTHORIZED, "unauthenticated", "Unauthorized");
    }

    let encoding = match request
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/proto")
    {
        value if value.contains("json") => Encoding::Json,
        _ => Encoding::Proto,
    };

    // Hyper decodes chunked transfer-encoding here, which minimal HTTP servers
    // often do not — without it callbacks look empty.
    let body = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return connect_error(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                &format!("could not read the callback body: {error}"),
            )
        }
    };

    let outcome = match path.as_str() {
        TOOL_ROUTE => handle_tool(&services, &body, encoding).await,
        STORE_ROUTE => handle_store(&services, &body, encoding).await,
        _ => {
            return connect_error(
                StatusCode::NOT_FOUND,
                "unimplemented",
                &format!("this SDK does not serve {path}"),
            )
        }
    };

    match outcome {
        Ok(payload) => Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_TYPE, encoding.content_type())
            .body(Full::new(payload))
            .expect("a well-formed response"),
        Err(message) => {
            tracing::warn!(target: "cursor_sdk::callback", %path, "{message}");
            connect_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", &message)
        }
    }
}

async fn handle_tool(
    services: &Services,
    body: &[u8],
    encoding: Encoding,
) -> std::result::Result<Bytes, String> {
    let call = match encoding {
        Encoding::Proto => {
            let request = proto::CallCustomToolRequest::decode(body)
                .map_err(|error| format!("could not decode CallCustomToolRequest: {error}"))?;
            ToolCall {
                name: request.tool_name,
                args: struct_to_json(request.args.as_ref()),
                tool_call_id: request.tool_call_id.filter(|value| !value.is_empty()),
                agent_id: request.agent_id,
            }
        }
        Encoding::Json => {
            let value: JsonValue = serde_json::from_slice(body)
                .map_err(|error| format!("could not parse the tool callback JSON: {error}"))?;
            ToolCall {
                name: value["toolName"].as_str().unwrap_or_default().to_string(),
                args: value.get("args").cloned().unwrap_or(JsonValue::Null),
                tool_call_id: value["toolCallId"].as_str().map(str::to_string),
                agent_id: value["agentId"].as_str().unwrap_or_default().to_string(),
            }
        }
    };

    tracing::debug!(
        target: "cursor_sdk::callback",
        tool = %call.name,
        agent_id = %call.agent_id,
        "running a custom tool"
    );

    let result = services
        .tools
        .call(call)
        .await
        .map_err(|error| error.to_string())?;

    Ok(match encoding {
        // A tool result is a Struct, so a scalar needs an object shell.
        Encoding::Proto => Bytes::from(
            proto::CallCustomToolResponse {
                result: Some(json_to_object_struct(result)),
            }
            .encode_to_vec(),
        ),
        Encoding::Json => {
            let wrapped = match result {
                JsonValue::Object(_) => result,
                other => json!({"value": other}),
            };
            Bytes::from(json!({"result": wrapped}).to_string())
        }
    })
}

async fn handle_store(
    services: &Services,
    body: &[u8],
    encoding: Encoding,
) -> std::result::Result<Bytes, String> {
    let Some(store) = services.store.as_ref() else {
        return Err("this SDK client was not configured with a custom agent store".to_string());
    };

    let request = match encoding {
        Encoding::Proto => {
            let request = proto::CallStoreRequest::decode(body)
                .map_err(|error| format!("could not decode CallStoreRequest: {error}"))?;
            StoreRequest::new(
                &request.substore,
                &request.method,
                struct_to_json(request.input.as_ref()),
            )
        }
        Encoding::Json => {
            let value: JsonValue = serde_json::from_slice(body)
                .map_err(|error| format!("could not parse the store callback JSON: {error}"))?;
            StoreRequest::new(
                value["substore"].as_str().unwrap_or_default(),
                value["method"].as_str().unwrap_or_default(),
                value.get("input").cloned().unwrap_or(JsonValue::Null),
            )
        }
    };

    let output = store
        .call(request)
        .await
        .map_err(|error| error.to_string())?;

    Ok(match encoding {
        Encoding::Proto => Bytes::from(
            proto::CallStoreResponse {
                // An unset output is the null result the bridge expects for a
                // get miss or a delete.
                output: output.as_ref().and_then(json_to_struct),
            }
            .encode_to_vec(),
        ),
        Encoding::Json => Bytes::from(match output {
            Some(value) => json!({"output": value}).to_string(),
            None => "{}".to_string(),
        }),
    })
}

fn connect_error(status: StatusCode, code: &str, message: &str) -> Response<Full<Bytes>> {
    let body = json!({"code": code, "message": message}).to_string();
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("a well-formed error response")
}

/// Compare secrets without an early exit on the first differing byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// Generate a bearer token for a callback server.
///
/// Uses the OS random source, falling back to a time- and address-derived seed
/// if that is unavailable. The token never leaves this machine: it is passed to
/// the bridge over the command line or a loopback RPC.
pub(crate) fn random_token() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    if fill_random(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let seed = nanos as u64 ^ ((&bytes as *const _ as u64) << 16) ^ std::process::id() as u64;
        let mut state = seed | 1;
        for chunk in bytes.chunks_mut(8) {
            // xorshift64*, seeded from entropy the process already has.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
        }
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(unix)]
fn fill_random(buffer: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buffer)
}

#[cfg(not(unix))]
fn fill_random(_buffer: &mut [u8]) -> std::io::Result<()> {
    Err(std::io::Error::other("no /dev/urandom on this platform"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::store::MemoryStore;
    use crate::options::CustomTool;

    async fn post(url: &str, path: &str, token: &str, body: JsonValue) -> (StatusCode, JsonValue) {
        let client: hyper_util::client::legacy::Client<
            hyper_util::client::legacy::connect::HttpConnector,
            Full<Bytes>,
        > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
        let request = Request::builder()
            .method("POST")
            .uri(format!("{url}{path}"))
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Full::new(Bytes::from(body.to_string())))
            .unwrap();
        let response = client.request(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);
        (status, value)
    }

    async fn server_with_tools() -> CallbackServer {
        let tools = ToolRegistry::new();
        tools.register(
            CustomTool::new("shout", "uppercases", json!({"type": "object"})),
            |call| async move {
                let text = call.string_arg("text").unwrap_or_default().to_uppercase();
                // Returned as a bare string on purpose: the server has to wrap it.
                Ok(JsonValue::String(text))
            },
        );
        CallbackServer::start(tools, Some(Arc::new(MemoryStore::new())), random_token())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn runs_a_tool_and_wraps_a_scalar_result() {
        let server = server_with_tools().await;
        let (status, body) = post(
            server.url(),
            TOOL_ROUTE,
            server.token(),
            json!({"toolName": "shout", "args": {"text": "hi"}, "agentId": "a1"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // A bare string cannot be encoded as a Struct, so it gets an object shell.
        assert_eq!(body, json!({"result": {"value": "HI"}}));
    }

    #[tokio::test]
    async fn rejects_a_callback_with_the_wrong_token() {
        let server = server_with_tools().await;
        let (status, body) = post(
            server.url(),
            TOOL_ROUTE,
            "not-the-token",
            json!({"toolName": "shout", "args": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], json!("unauthenticated"));
    }

    #[tokio::test]
    async fn an_unknown_tool_fails_the_callback() {
        let server = server_with_tools().await;
        let (status, body) = post(
            server.url(),
            TOOL_ROUTE,
            server.token(),
            json!({"toolName": "absent", "args": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["message"].as_str().unwrap().contains("absent"));
    }

    #[tokio::test]
    async fn serves_store_callbacks_on_the_same_port() {
        let server = server_with_tools().await;
        let (status, body) = post(
            server.url(),
            STORE_ROUTE,
            server.token(),
            json!({
                "substore": "agents",
                "method": "create",
                "input": {"agent": {"agentId": "a1", "name": "demo"}},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"output": {"agentId": "a1", "name": "demo"}}));
    }

    #[tokio::test]
    async fn a_get_miss_returns_an_unset_output() {
        let server = server_with_tools().await;
        let (status, body) = post(
            server.url(),
            STORE_ROUTE,
            server.token(),
            json!({"substore": "agents", "method": "get", "input": {"agentId": "absent"}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({}), "an unset output is the null result");
    }

    #[tokio::test]
    async fn unknown_routes_are_reported_as_unimplemented() {
        let server = server_with_tools().await;
        let (status, body) = post(
            server.url(),
            "/sdk.v1.SomethingElse/Method",
            server.token(),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], json!("unimplemented"));
    }

    #[tokio::test]
    async fn stops_when_dropped() {
        let server = server_with_tools().await;
        let url = server.url().to_string();
        let token = server.token().to_string();
        drop(server);
        // Give the accept loop a moment to observe the shutdown signal.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client: hyper_util::client::legacy::Client<
            hyper_util::client::legacy::connect::HttpConnector,
            Full<Bytes>,
        > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();
        let request = Request::builder()
            .method("POST")
            .uri(format!("{url}{TOOL_ROUTE}"))
            .header(hyper::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(client.request(request).await.is_err());
    }

    #[test]
    fn tokens_are_long_and_unique() {
        let first = random_token();
        assert!(first.len() >= 40);
        assert_ne!(first, random_token());
    }

    #[test]
    fn token_comparison_rejects_length_mismatches() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }
}
