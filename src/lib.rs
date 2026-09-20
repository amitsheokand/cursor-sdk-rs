//! A Rust SDK for [Cursor](https://cursor.com) agents.
//!
//! This crate drives Cursor agents — local ones that run against a working
//! directory on this machine, and cloud ones that run against a git repository
//! — through the `cursor-sdk-bridge` process and its stable `sdk.v1`
//! Connect/protobuf contract. The bridge is an implementation detail: it is
//! found, launched, handshaken, and stopped for you.
//!
//! # Getting started
//!
//! You need two things:
//!
//! 1. A Cursor API key, from the
//!    [dashboard](https://cursor.com/dashboard/api). Export it as
//!    `CURSOR_API_KEY` and the client picks it up.
//! 2. The `cursor-sdk-bridge` executable. The shortest path is
//!    `cargo install cursor-sdk-bridge-fetch && cursor-sdk-bridge-fetch`, which
//!    installs a checksum-verified build to `~/.cursor/sdk-bridge/` — somewhere
//!    this crate already looks. Alternatively `pip install cursor-sdk` puts one
//!    on `PATH`, or download a standalone archive from
//!    [`cursor/sdk-bridge` releases](https://github.com/cursor/sdk-bridge/releases)
//!    and point `CURSOR_SDK_BRIDGE_BIN` at `bin/cursor-sdk-bridge`.
//!
//! This crate never downloads anything itself: provisioning lives in a separate
//! tool so the library compiles without a TLS stack.
//!
//! Then one prompt is one call:
//!
//! ```no_run
//! use cursor_sdk::{AgentOptions, Client};
//!
//! # async fn demo() -> cursor_sdk::Result<()> {
//! let client = Client::new();
//! let answer = client
//!     .prompt(AgentOptions::local(".").model("composer-2.5"), "What does this repo do?")
//!     .await?;
//! println!("{answer}");
//! client.close().await?;
//! # Ok(()) }
//! ```
//!
//! # Streaming a turn
//!
//! [`Agent::send`] returns as soon as the stream opens, so output can be shown
//! while the agent is still working:
//!
//! ```no_run
//! use cursor_sdk::{AgentOptions, Client, RunEvent};
//!
//! # async fn demo() -> cursor_sdk::Result<()> {
//! let client = Client::new();
//! let agent = client
//!     .create_agent(AgentOptions::local("/path/to/repo").model("composer-2.5"))
//!     .await?;
//!
//! let mut run = agent.send("Add a test for the parser.").await?;
//! while let Some(event) = run.next_event().await {
//!     match event? {
//!         RunEvent::Message(message) => {
//!             if let Some(text) = message.text() {
//!                 print!("{text}");
//!             }
//!         }
//!         RunEvent::Completed(outcome) => println!("\n[{}]", outcome.status),
//!         _ => {}
//!     }
//! }
//!
//! agent.close().await?;
//! client.close().await?;
//! # Ok(()) }
//! ```
//!
//! # The shape of the API
//!
//! | Type | Role |
//! | --- | --- |
//! | [`Client`] | Owns the bridge process and the transport. Cheap to clone; spawns nothing until first use. |
//! | [`Agent`] | One conversation: send, manage, inspect. |
//! | [`Run`] | One turn: stream events, wait for the outcome, cancel, resume. |
//! | [`AgentOptions`] / [`SendOptions`] | How to create an agent and how to send a message. |
//! | [`Error`] | Every failure, classified by [`ErrorKind`]. |
//! | [`ToolRegistry`] | Rust functions the agent can call. |
//! | [`AgentStore`] | Durable local agent state owned by your process. |
//!
//! # Errors
//!
//! Failures are classified, not stringly typed. Branch on [`Error::kind`]:
//!
//! ```no_run
//! # async fn demo(client: &cursor_sdk::Client) {
//! use cursor_sdk::ErrorKind;
//!
//! match client.models().await {
//!     Ok(models) => println!("{} models", models.len()),
//!     Err(error) if error.kind() == Some(ErrorKind::Unauthenticated) => {
//!         eprintln!("check CURSOR_API_KEY");
//!     }
//!     Err(error) => {
//!         // Always log the full request id; Cursor support traces by it.
//!         eprintln!("{error} (request {:?})", error.request_id());
//!     }
//! }
//! # }
//! ```
//!
//! # Lifetimes and cleanup
//!
//! [`Client::close`] stops the bridge gracefully. If a client is dropped
//! without it — including on a panic — the process is still killed, so a bridge
//! cannot outlive the program on any normal path. A signal is the exception, so
//! binaries that care can call [`install_exit_guard`].
//!
//! # Custom tools
//!
//! Register a Rust function and the agent can call it:
//!
//! ```no_run
//! use cursor_sdk::{AgentOptions, Client, CustomTool};
//! use serde_json::json;
//!
//! # async fn demo() -> cursor_sdk::Result<()> {
//! let client = Client::builder()
//!     .register_tool(
//!         CustomTool::new(
//!             "ticket_status",
//!             "Look up the status of an internal ticket",
//!             json!({
//!                 "type": "object",
//!                 "properties": {"id": {"type": "string"}},
//!                 "required": ["id"],
//!             }),
//!         ),
//!         |call| async move {
//!             let id = call.string_arg("id").unwrap_or_default().to_string();
//!             Ok(json!({"id": id, "status": "open"}))
//!         },
//!     )
//!     .build();
//!
//! let agent = client
//!     .create_agent(AgentOptions::local(".").model("composer-2.5"))
//!     .await?;
//! println!("{}", agent.ask("What is the status of ticket AB-1?").await?);
//! # Ok(()) }
//! ```
//!
//! # Relationship to the first-party SDKs
//!
//! Cursor publishes and supports the `sdk.v1` protos and the bridge binaries.
//! This crate is an adapter built on that contract, not a first-party SDK. If
//! you write TypeScript or Python, use `@cursor/sdk` or `cursor-sdk` instead.

#![warn(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod agent;
pub mod bridge;
pub mod callback;
pub mod client;
pub mod error;
pub mod json;
pub mod manifest;
pub mod options;
pub mod proto;
pub mod run;
pub mod types;

pub(crate) mod transport;

pub use agent::{Agent, AgentGuard};
pub use bridge::{install_exit_guard, BridgeInfo, BridgeOptions, CallbackEndpoint, LocalStore};
pub use callback::store::{
    AgentStore, LoggingStore, MemoryStore, StoreMethod, StoreRequest, Substore,
};
pub use callback::tools::{ToolCall, ToolRegistry};
pub use callback::{CallbackServer, HandlerError, HandlerResult};
pub use client::{Client, ClientBuilder, ListAgents, ListMessages, ListRuns, RuntimeFilter};
pub use error::{BridgeError, Error, ErrorKind, RateLimit, Result, RpcError};
pub use manifest::BridgeManifest;
pub use options::{
    AgentMode, AgentOptions, CloudAgent, CloudRepository, CustomTool, Image, LocalAgent, McpAuth,
    McpServer, McpTransport, Prompt, SendOptions, SettingSource, SubAgent, SubAgentMcpServer,
};
pub use run::{Run, RunEvent, StreamMessage};
pub use transport::ServerStream;
pub use types::{
    AgentInfo, AgentMessage, AgentRuntime, AgentStatus, AgentUsage, Artifact, BridgeVersion,
    CloudEnvironment, CloudEnvironmentKind, GitBranch, Model, ModelChoice, ModelParameter,
    ModelParameterOption, ModelVariant, Page, Repository, RunOutcome, RunStatus, RunUsage,
    TokenUsage, UsageCost, User,
};

/// The `@cursor/sdk` version of the vendored `sdk.v1` contract.
pub const CONTRACT_SDK_VERSION: &str = proto::CONTRACT_SDK_VERSION;

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: &str = proto::PROTOCOL_VERSION;
