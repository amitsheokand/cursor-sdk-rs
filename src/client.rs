//! The client: owns the bridge, the transport, and every RPC.
//!
//! A [`Client`] is cheap to clone (it is an `Arc` inside) and starts nothing
//! until the first call that needs the bridge. That means a program which
//! builds a client and never uses it never spawns a process, and a program
//! which uses it from several tasks shares one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio::sync::{Mutex, OnceCell};

use crate::agent::Agent;
use crate::bridge::{Bridge, BridgeInfo, BridgeOptions, CallbackEndpoint, LocalStore};
use crate::callback::server::random_token;
use crate::callback::store::AgentStore;
use crate::callback::tools::{ToolCall, ToolRegistry};
use crate::callback::{CallbackServer, HandlerResult};
use crate::error::{Error, Result};
use crate::options::{AgentOptions, CustomTool, Prompt, SendOptions};
use crate::proto;
use crate::run::Run;
use crate::transport::{ServerStream, Transport};
use crate::types::{
    AgentInfo, AgentMessage, AgentUsage, Artifact, BridgeVersion, Model, Page, Repository,
    RunOutcome, User,
};

const AGENT_SERVICE: &str = "SdkAgentService";
const CURSOR_SERVICE: &str = "SdkCursorService";
const CONTROL_SERVICE: &str = "SdkBridgeControlService";

/// Which runtime an operation should address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeFilter {
    /// Let the bridge decide from the agent's options and context.
    #[default]
    Auto,
    /// Agents running beside the bridge.
    Local,
    /// Agents running in Cursor's cloud.
    Cloud,
}

impl RuntimeFilter {
    fn to_proto(self) -> i32 {
        let value = match self {
            RuntimeFilter::Auto => proto::Runtime::Auto,
            RuntimeFilter::Local => proto::Runtime::Local,
            RuntimeFilter::Cloud => proto::Runtime::Cloud,
        };
        value as i32
    }
}

/// Filters and pagination for [`Client::list_agents`].
#[derive(Debug, Clone, Default)]
pub struct ListAgents {
    limit: u32,
    cursor: Option<String>,
    runtime: RuntimeFilter,
    cwd: Option<PathBuf>,
    pr_url: Option<String>,
    include_archived: Option<bool>,
    api_key: Option<String>,
}

impl ListAgents {
    /// No filters: the default listing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Maximum number of agents per page.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Continue from a previous page's [`Page::next_cursor`].
    #[must_use]
    pub fn cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Restrict to one runtime.
    #[must_use]
    pub fn runtime(mut self, runtime: RuntimeFilter) -> Self {
        self.runtime = runtime;
        self
    }

    /// Restrict to local agents rooted at this directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = Some(cwd.as_ref().to_path_buf());
        self
    }

    /// Restrict to cloud agents associated with this pull request.
    #[must_use]
    pub fn pr_url(mut self, pr_url: impl Into<String>) -> Self {
        self.pr_url = Some(pr_url.into());
        self
    }

    /// Include archived agents, which are hidden by default.
    #[must_use]
    pub fn include_archived(mut self, include: bool) -> Self {
        self.include_archived = Some(include);
        self
    }

    /// Override the client's API key for this call.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    fn to_proto(&self, default_api_key: Option<&str>) -> proto::ListAgentsOptions {
        proto::ListAgentsOptions {
            limit: self.limit,
            cursor: self.cursor.clone().unwrap_or_default(),
            runtime: self.runtime.to_proto(),
            cwd: path_string(self.cwd.as_deref()),
            pr_url: self.pr_url.clone().unwrap_or_default(),
            include_archived: self.include_archived,
            api_key: pick_key(self.api_key.as_deref(), default_api_key),
        }
    }
}

/// Filters and pagination for [`Agent::runs`](crate::Agent::runs).
#[derive(Debug, Clone, Default)]
pub struct ListRuns {
    limit: u32,
    cursor: Option<String>,
    runtime: RuntimeFilter,
    cwd: Option<PathBuf>,
    api_key: Option<String>,
}

impl ListRuns {
    /// No filters: the default listing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Maximum number of runs per page.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Continue from a previous page's [`Page::next_cursor`].
    #[must_use]
    pub fn cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Restrict to one runtime.
    #[must_use]
    pub fn runtime(mut self, runtime: RuntimeFilter) -> Self {
        self.runtime = runtime;
        self
    }

    pub(crate) fn to_proto(
        &self,
        default_cwd: Option<&Path>,
        default_api_key: Option<&str>,
    ) -> proto::ListRunsOptions {
        proto::ListRunsOptions {
            limit: self.limit,
            cursor: self.cursor.clone().unwrap_or_default(),
            runtime: self.runtime.to_proto(),
            cwd: path_string(self.cwd.as_deref().or(default_cwd)),
            api_key: pick_key(self.api_key.as_deref(), default_api_key),
        }
    }
}

/// Pagination for [`Agent::messages`](crate::Agent::messages).
#[derive(Debug, Clone, Default)]
pub struct ListMessages {
    limit: u32,
    offset: u32,
    runtime: RuntimeFilter,
}

impl ListMessages {
    /// Every message from the start.
    pub fn new() -> Self {
        Self::default()
    }

    /// Maximum number of messages to return.
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Skip this many messages.
    #[must_use]
    pub fn offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }

    pub(crate) fn to_proto(
        &self,
        default_cwd: Option<&Path>,
        default_api_key: Option<&str>,
    ) -> proto::GetAgentMessagesOptions {
        proto::GetAgentMessagesOptions {
            limit: self.limit,
            offset: self.offset,
            runtime: self.runtime.to_proto(),
            cwd: path_string(default_cwd),
            api_key: pick_key(None, default_api_key),
        }
    }
}

fn path_string(path: Option<&Path>) -> String {
    path.map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn pick_key(explicit: Option<&str>, fallback: Option<&str>) -> String {
    explicit.or(fallback).unwrap_or_default().to_string()
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    bridge: BridgeOptions,
    attach: Option<(String, String)>,
    api_key: Option<String>,
    request_timeout: Duration,
    tools: ToolRegistry,
    store: Option<Arc<dyn AgentStore>>,
    verify_on_connect: bool,
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientBuilder")
            .field("bridge", &self.bridge)
            .field("attached", &self.attach.as_ref().map(|(url, _)| url))
            .field("has_api_key", &self.api_key.is_some())
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            bridge: BridgeOptions::default(),
            attach: None,
            // `CURSOR_API_KEY` is the conventional place to keep it, so a
            // program that exports it needs no configuration at all.
            api_key: std::env::var("CURSOR_API_KEY")
                .ok()
                .filter(|key| !key.is_empty()),
            request_timeout: Duration::from_secs(60),
            tools: ToolRegistry::new(),
            store: None,
            verify_on_connect: true,
        }
    }
}

impl ClientBuilder {
    /// The Cursor API key.
    ///
    /// It is put in the bridge's environment *and* set explicitly on every
    /// request that accepts one — `docs/protocol.md` is emphatic that the
    /// environment variable alone is not enough on every bridge build.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// The workspace root: the bridge's `--workspace`.
    #[must_use]
    pub fn workspace(mut self, workspace: impl AsRef<Path>) -> Self {
        self.bridge.workspace = Some(workspace.as_ref().to_path_buf());
        self
    }

    /// Use a specific `cursor-sdk-bridge` executable.
    ///
    /// Without this the binary is found via `CURSOR_SDK_BRIDGE_BIN`, then
    /// `PATH`, then `~/.cursor/sdk-bridge/bin/`.
    #[must_use]
    pub fn bridge_binary(mut self, path: impl AsRef<Path>) -> Self {
        self.bridge.binary = Some(path.as_ref().to_path_buf());
        self
    }

    /// Attach to a bridge somebody else is running instead of spawning one.
    ///
    /// The client will not stop that process on [`Client::close`].
    #[must_use]
    pub fn endpoint(mut self, url: impl Into<String>, token: impl Into<String>) -> Self {
        self.attach = Some((url.into(), token.into()));
        self
    }

    /// Deadline for a unary RPC, and for a stream's response headers.
    ///
    /// It never bounds how long a stream may stay open: a run can idle for as
    /// long as a tool call takes.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// How long to wait for the bridge's ready line.
    #[must_use]
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.bridge.startup_timeout = timeout;
        self
    }

    /// How long a graceful shutdown may take before the process is killed.
    #[must_use]
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.bridge.shutdown_timeout = timeout;
        self
    }

    /// How long the bridge may drain in-flight RPCs during `Shutdown`.
    ///
    /// Zero — the default — means exit immediately. Anything else must be
    /// shorter than [`ClientBuilder::shutdown_timeout`], or the bridge is
    /// killed while it is still draining.
    #[must_use]
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.bridge.shutdown_grace = grace;
        self
    }

    /// Where the bridge keeps durable local agent state.
    #[must_use]
    pub fn local_store(mut self, store: LocalStore) -> Self {
        self.bridge.local_store = Some(store);
        self
    }

    /// Own local agent state in this process.
    ///
    /// Implies [`LocalStore::Custom`] and starts a store callback server before
    /// the bridge launches, which is the only time the bridge can be told about
    /// one.
    #[must_use]
    pub fn agent_store(mut self, store: impl AgentStore) -> Self {
        self.store = Some(Arc::new(store));
        self.bridge.local_store = Some(LocalStore::Custom);
        self
    }

    /// Share an existing [`ToolRegistry`], for example across several clients.
    #[must_use]
    pub fn tool_registry(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// Register a custom tool. See [`ToolRegistry::register`].
    #[must_use]
    pub fn register_tool<F, Fut>(self, definition: CustomTool, handler: F) -> Self
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = HandlerResult<JsonValue>> + Send + 'static,
    {
        self.tools.register(definition, handler);
        self
    }

    /// An extra environment variable for the bridge process.
    #[must_use]
    pub fn bridge_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.bridge.env.insert(key.into(), value.into());
        self
    }

    /// An extra command-line argument for the bridge process.
    #[must_use]
    pub fn bridge_arg(mut self, arg: impl Into<String>) -> Self {
        self.bridge.extra_args.push(arg.into());
        self
    }

    /// Make the bridge log every RPC's name, outcome, duration, and error to
    /// its stderr, which this crate forwards to `tracing` at `debug` level on
    /// the `cursor_sdk::bridge` target. Payloads are never logged.
    #[must_use]
    pub fn verbose(mut self, verbose: bool) -> Self {
        if verbose {
            self.bridge.extra_args.push("--verbose".to_string());
            self.bridge
                .env
                .insert("CURSOR_SDK_BRIDGE_LOG".to_string(), "1".to_string());
        }
        self
    }

    /// The language reported to Cursor for traffic attribution.
    #[must_use]
    pub fn client_language(mut self, language: impl Into<String>) -> Self {
        self.bridge.client_language = language.into();
        self
    }

    /// Skip the `Ping` and `GetVersion` handshake check on first connect.
    ///
    /// The check costs two loopback round-trips and turns a bad token or a
    /// mismatched protocol into an error at connect time rather than midway
    /// through a turn. Disable it only if you have measured that it matters.
    #[must_use]
    pub fn verify_on_connect(mut self, verify: bool) -> Self {
        self.verify_on_connect = verify;
        self
    }

    /// Finish the client. Nothing is spawned until the first RPC.
    pub fn build(self) -> Client {
        Client {
            inner: Arc::new(Inner {
                bridge_options: self.bridge,
                attach: self.attach,
                api_key: self.api_key,
                request_timeout: self.request_timeout,
                tools: self.tools,
                store: self.store,
                verify_on_connect: self.verify_on_connect,
                connection: OnceCell::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }
}

struct Connection {
    transport: Transport,
    bridge: Mutex<Bridge>,
    /// Kept alive for as long as the bridge is: dropping it stops the server.
    _callbacks: Option<CallbackServer>,
}

struct Inner {
    bridge_options: BridgeOptions,
    attach: Option<(String, String)>,
    api_key: Option<String>,
    request_timeout: Duration,
    tools: ToolRegistry,
    store: Option<Arc<dyn AgentStore>>,
    verify_on_connect: bool,
    connection: OnceCell<Connection>,
    closed: AtomicBool,
}

/// The entry point: a handle on a Cursor SDK bridge.
///
/// ```no_run
/// # async fn demo() -> cursor_sdk::Result<()> {
/// use cursor_sdk::{AgentOptions, Client};
///
/// let client = Client::new();
/// let agent = client.create_agent(AgentOptions::local(".").model("composer-2.5")).await?;
/// let answer = agent.send("Summarize this repository.").await?.text().await?;
/// println!("{answer}");
/// client.close().await?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field(
                "endpoint",
                &self
                    .inner
                    .connection
                    .get()
                    .map(|connection| connection.transport.base_url()),
            )
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    /// A client with default settings, taking the API key from
    /// `CURSOR_API_KEY`.
    pub fn new() -> Self {
        Client::builder().build()
    }

    /// Configure a client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// The custom tools this client offers to its agents.
    pub fn tools(&self) -> &ToolRegistry {
        &self.inner.tools
    }

    /// Register a custom tool, before or after the bridge starts.
    ///
    /// Registering after the bridge is running re-points it at this client's
    /// callback server via `SetToolCallback`. Note that a tool only reaches a
    /// model if its declaration was included in the agent's options, so
    /// register before creating the agent that should use it.
    pub async fn register_tool<F, Fut>(&self, definition: CustomTool, handler: F) -> Result<()>
    where
        F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = HandlerResult<JsonValue>> + Send + 'static,
    {
        self.inner.tools.register(definition, handler);
        if let Some(connection) = self.inner.connection.get() {
            if let Some(callbacks) = &connection._callbacks {
                self.unary::<_, proto::SetToolCallbackResponse>(
                    CONTROL_SERVICE,
                    "SetToolCallback",
                    &proto::SetToolCallbackRequest {
                        url: callbacks.url().to_string(),
                        auth_token: callbacks.token().to_string(),
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The API key this client sends, if it has one.
    pub fn api_key(&self) -> Option<&str> {
        self.inner.api_key.as_deref()
    }

    // ---- bridge lifecycle -------------------------------------------------

    async fn connection(&self) -> Result<&Connection> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(Error::Config(
                "this client has been closed; build a new one".to_string(),
            ));
        }
        self.inner
            .connection
            .get_or_try_init(|| self.connect())
            .await
    }

    async fn connect(&self) -> Result<Connection> {
        let mut options = self.inner.bridge_options.clone();
        options.api_key = self.inner.api_key.clone();

        // The callback server must exist before the bridge launches: a custom
        // store can only be configured with launch flags, because agents may
        // load state before any RPC arrives.
        let needs_callbacks = self.inner.store.is_some() || !self.inner.tools.is_empty();
        let callbacks = if needs_callbacks {
            let server = CallbackServer::start(
                self.inner.tools.clone(),
                self.inner.store.clone(),
                random_token(),
            )
            .await?;
            let endpoint = CallbackEndpoint {
                url: server.url().to_string(),
                auth_token: server.token().to_string(),
            };
            if !self.inner.tools.is_empty() {
                options.tool_callback = Some(endpoint.clone());
            }
            if self.inner.store.is_some() {
                options.store_callback = Some(endpoint);
            }
            Some(server)
        } else {
            None
        };

        let bridge = match &self.inner.attach {
            Some((url, token)) => {
                if self.inner.store.is_some() {
                    return Err(Error::Config(
                        "a custom agent store can only be configured when this client launches \
                         the bridge, not when attaching to one"
                            .to_string(),
                    ));
                }
                Bridge::attach(url.clone(), token.clone())
            }
            None => {
                options.validate()?;
                Bridge::spawn(&options).await?
            }
        };

        let transport = Transport::new(
            bridge.endpoint(),
            bridge.token(),
            self.inner.request_timeout,
        )?;

        let connection = Connection {
            transport,
            bridge: Mutex::new(bridge),
            _callbacks: callbacks,
        };

        if self.inner.verify_on_connect {
            self.verify(&connection).await?;
        }
        Ok(connection)
    }

    /// Confirm the handshake worked before the caller's first real call.
    async fn verify(&self, connection: &Connection) -> Result<()> {
        connection
            .transport
            .unary::<_, proto::PingResponse>(CONTROL_SERVICE, "Ping", &proto::PingRequest {})
            .await?;

        let version: BridgeVersion = connection
            .transport
            .unary::<_, proto::GetVersionResponse>(
                CONTROL_SERVICE,
                "GetVersion",
                &proto::GetVersionRequest {},
            )
            .await?
            .into();

        if !version.speaks_supported_protocol() {
            // Not fatal: sdk.v1 only changes additively, and an unexpected
            // string is more likely a newer bridge than a broken one.
            tracing::warn!(
                target: "cursor_sdk",
                bridge_protocol = %version.protocol_version,
                expected = %proto::PROTOCOL_VERSION,
                "the bridge reports a different protocol version than this SDK was generated from"
            );
        }
        tracing::debug!(
            target: "cursor_sdk",
            bridge_version = %version.bridge_version,
            capabilities = ?version.capabilities,
            "connected to the bridge"
        );
        Ok(())
    }

    /// The endpoint the client talks to, connecting first if needed.
    pub async fn endpoint(&self) -> Result<String> {
        Ok(self.connection().await?.transport.base_url().to_string())
    }

    /// What the bridge reported during the handshake.
    ///
    /// `None` when the client attached to an endpoint instead of spawning.
    pub async fn bridge_info(&self) -> Result<Option<BridgeInfo>> {
        let connection = self.connection().await?;
        let bridge = connection.bridge.lock().await;
        Ok(bridge.info().cloned())
    }

    /// Stop the bridge: `Shutdown`, then wait, then kill.
    ///
    /// Attached bridges are left running. Dropping the last [`Client`] clone
    /// without calling this still kills a managed bridge, so a leak is not
    /// possible on a normal path — but this is the graceful version, and the
    /// one that reports failures.
    pub async fn close(&self) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let Some(connection) = self.inner.connection.get() else {
            return Ok(());
        };
        let mut bridge = connection.bridge.lock().await;
        if !bridge.is_managed() {
            return Ok(());
        }

        // A failure here is not fatal: wait_for_exit escalates to a kill.
        if let Err(error) = connection
            .transport
            .unary::<_, proto::ShutdownResponse>(
                CONTROL_SERVICE,
                "Shutdown",
                &proto::ShutdownRequest {
                    grace_seconds: self.inner.bridge_options.shutdown_grace.as_secs() as u32,
                },
            )
            .await
        {
            tracing::debug!(
                target: "cursor_sdk::bridge",
                %error,
                "the Shutdown RPC failed; falling back to waiting and killing"
            );
        }
        bridge.wait_for_exit().await;
        Ok(())
    }

    // ---- raw transport ----------------------------------------------------

    async fn unary<Req, Res>(&self, service: &str, method: &str, request: &Req) -> Result<Res>
    where
        Req: prost::Message,
        Res: prost::Message + Default,
    {
        self.connection()
            .await?
            .transport
            .unary(service, method, request)
            .await
    }

    /// Call any unary RPC by name, with the generated request and response
    /// types from [`crate::proto`].
    ///
    /// The escape hatch for an RPC this crate has not wrapped yet — for
    /// instance one a newer bridge added. Ordinary code should not need it.
    pub async fn call_unary<Req, Res>(
        &self,
        service: &str,
        method: &str,
        request: &Req,
    ) -> Result<Res>
    where
        Req: prost::Message,
        Res: prost::Message + Default,
    {
        self.unary(service, method, request).await
    }

    /// Open any server-streaming RPC by name.
    ///
    /// The streaming counterpart of [`Client::call_unary`].
    pub async fn call_server_stream<Req, Res>(
        &self,
        service: &str,
        method: &str,
        request: &Req,
    ) -> Result<ServerStream<Res>>
    where
        Req: prost::Message,
        Res: prost::Message + Default,
    {
        self.connection()
            .await?
            .transport
            .server_stream(service, method, request)
            .await
    }

    // ---- bridge control ---------------------------------------------------

    /// Liveness check. Returns the bridge's fixed reply.
    pub async fn ping(&self) -> Result<String> {
        let response: proto::PingResponse = self
            .unary(CONTROL_SERVICE, "Ping", &proto::PingRequest {})
            .await?;
        Ok(response.message)
    }

    /// The bridge's build version, protocol version, and capabilities.
    ///
    /// Gate optional features on
    /// [`BridgeVersion::has_capability`] rather than on a version comparison.
    pub async fn version(&self) -> Result<BridgeVersion> {
        let response: proto::GetVersionResponse = self
            .unary(CONTROL_SERVICE, "GetVersion", &proto::GetVersionRequest {})
            .await?;
        Ok(response.into())
    }

    // ---- catalog ----------------------------------------------------------

    /// The account the API key belongs to.
    pub async fn me(&self) -> Result<User> {
        let response: proto::MeResponse = self
            .unary(
                CURSOR_SERVICE,
                "Me",
                &proto::MeRequest {
                    options: Some(self.cursor_options()?),
                },
            )
            .await?;
        Ok(response.user.unwrap_or_default().into())
    }

    /// The models available to the account.
    ///
    /// Local agents need an explicit model, and this is where the ids come
    /// from.
    pub async fn models(&self) -> Result<Vec<Model>> {
        let response: proto::ListModelsResponse = self
            .unary(
                CURSOR_SERVICE,
                "ListModels",
                &proto::ListModelsRequest {
                    options: Some(self.cursor_options()?),
                },
            )
            .await?;
        Ok(response.items.into_iter().map(Model::from).collect())
    }

    /// The repositories usable with cloud agents.
    pub async fn repositories(&self) -> Result<Vec<Repository>> {
        let response: proto::ListRepositoriesResponse = self
            .unary(
                CURSOR_SERVICE,
                "ListRepositories",
                &proto::ListRepositoriesRequest {
                    options: Some(self.cursor_options()?),
                },
            )
            .await?;
        Ok(response.items.into_iter().map(Repository::from).collect())
    }

    /// Catalog calls hard-require a per-call key: current bridges fail closed
    /// rather than falling back to `CURSOR_API_KEY`.
    fn cursor_options(&self) -> Result<proto::CursorRequestOptions> {
        let api_key = self.inner.api_key.clone().unwrap_or_default();
        if api_key.is_empty() {
            return Err(Error::Config(
                "catalog calls require a Cursor API key on the request; set it with \
                 Client::builder().api_key(..) or the CURSOR_API_KEY environment variable"
                    .to_string(),
            ));
        }
        Ok(proto::CursorRequestOptions { api_key })
    }

    // ---- agents -----------------------------------------------------------

    /// Create an agent.
    pub async fn create_agent(&self, options: AgentOptions) -> Result<Agent> {
        self.create_agent_with_key(options, None).await
    }

    /// Create an agent, retrying safely with an idempotency key.
    ///
    /// Only meaningful for cloud agents: the key lets the backend collapse a
    /// retry onto the original agent instead of creating a second one.
    pub async fn create_agent_idempotent(
        &self,
        options: AgentOptions,
        idempotency_key: impl Into<String>,
    ) -> Result<Agent> {
        self.create_agent_with_key(options, Some(idempotency_key.into()))
            .await
    }

    async fn create_agent_with_key(
        &self,
        options: AgentOptions,
        idempotency_key: Option<String>,
    ) -> Result<Agent> {
        let (request_options, cwd) = self.prepare_agent_options(options)?;
        let response: proto::CreateAgentResponse = self
            .unary(
                AGENT_SERVICE,
                "CreateAgent",
                &proto::CreateAgentRequest {
                    options: Some(request_options),
                    idempotency_key,
                },
            )
            .await?;

        Ok(Agent::new(
            self.clone(),
            response.agent_id,
            response
                .model
                .and_then(crate::types::ModelChoice::from_proto),
            cwd,
        ))
    }

    /// Re-attach to an existing agent, applying updated options.
    pub async fn resume_agent(
        &self,
        agent_id: impl Into<String>,
        options: AgentOptions,
    ) -> Result<Agent> {
        let (request_options, cwd) = self.prepare_agent_options(options)?;
        let response: proto::ResumeAgentResponse = self
            .unary(
                AGENT_SERVICE,
                "ResumeAgent",
                &proto::ResumeAgentRequest {
                    agent_id: agent_id.into(),
                    options: Some(request_options),
                },
            )
            .await?;
        Ok(Agent::new(
            self.clone(),
            response.agent_id,
            response
                .model
                .and_then(crate::types::ModelChoice::from_proto),
            cwd,
        ))
    }

    /// A handle on an agent that already exists, without an RPC.
    ///
    /// Use [`Client::resume_agent`] when the agent's options need updating, or
    /// [`Client::get_agent`] when you want its metadata.
    pub fn agent(&self, agent_id: impl Into<String>) -> Agent {
        Agent::new(self.clone(), agent_id.into(), None, None)
    }

    /// Merge in the client's defaults: API key, and the custom tools this
    /// client offers.
    fn prepare_agent_options(
        &self,
        mut options: AgentOptions,
    ) -> Result<(proto::AgentOptions, Option<PathBuf>)> {
        if let Some(local) = options.local_mut() {
            // A tool the model can call but this client cannot execute is a
            // failed turn, so the declarations follow the registry.
            local.merge_tools(self.inner.tools.definitions());
        }
        if options.uses_custom_store() && self.inner.store.is_none() {
            return Err(Error::Config(
                "this agent asks for a custom store, but the client has none; configure it with \
                 Client::builder().agent_store(..) before the bridge launches"
                    .to_string(),
            ));
        }
        let cwd = options.cwd();
        let encoded = options.finish(self.inner.api_key.as_deref())?;
        Ok((encoded, cwd))
    }

    /// Metadata for one agent.
    pub async fn get_agent(&self, agent_id: impl Into<String>) -> Result<AgentInfo> {
        let response: proto::GetAgentResponse = self
            .unary(
                AGENT_SERVICE,
                "GetAgent",
                &proto::GetAgentRequest {
                    agent_id: agent_id.into(),
                    options: Some(self.agent_operation_options(None)),
                },
            )
            .await?;
        Ok(response.agent.unwrap_or_default().into())
    }

    /// One page of agents.
    pub async fn list_agents(&self, filter: ListAgents) -> Result<Page<AgentInfo>> {
        let response: proto::ListAgentsResponse = self
            .unary(
                AGENT_SERVICE,
                "ListAgents",
                &proto::ListAgentsRequest {
                    options: Some(filter.to_proto(self.inner.api_key.as_deref())),
                },
            )
            .await?;
        Ok(Page::new(
            response.items.into_iter().map(AgentInfo::from).collect(),
            response.next_cursor,
        ))
    }

    /// Every agent matching the filter, following pagination cursors.
    ///
    /// Convenient, but it fetches every page: prefer [`Client::list_agents`]
    /// with a limit when the account has many agents.
    pub async fn list_all_agents(&self, filter: ListAgents) -> Result<Vec<AgentInfo>> {
        let mut collected = Vec::new();
        let mut filter = filter;
        loop {
            let page = self.list_agents(filter.clone()).await?;
            collected.extend(page.items);
            match page.next_cursor {
                Some(cursor) => filter = filter.cursor(cursor),
                None => return Ok(collected),
            }
        }
    }

    pub(crate) fn agent_operation_options(
        &self,
        cwd: Option<&Path>,
    ) -> proto::AgentOperationOptions {
        proto::AgentOperationOptions {
            cwd: path_string(cwd),
            api_key: self.inner.api_key.clone().unwrap_or_default(),
        }
    }

    // ---- runs -------------------------------------------------------------

    pub(crate) async fn send_stream(
        &self,
        agent_id: &str,
        prompt: &Prompt,
        options: &SendOptions,
        idempotency_key: Option<String>,
    ) -> Result<ServerStream<proto::RunStreamMessage>> {
        self.connection()
            .await?
            .transport
            .server_stream(
                AGENT_SERVICE,
                "Send",
                &proto::SendRequest {
                    agent_id: agent_id.to_string(),
                    message: Some(prompt.to_proto()),
                    options: Some(options.to_proto()),
                    idempotency_key,
                },
            )
            .await
    }

    pub(crate) async fn observe_run_stream(
        &self,
        run_id: &str,
        after_offset: Option<&str>,
    ) -> Result<ServerStream<proto::RunStreamMessage>> {
        self.connection()
            .await?
            .transport
            .server_stream(
                AGENT_SERVICE,
                "ObserveRun",
                &proto::ObserveRunRequest {
                    run_id: run_id.to_string(),
                    after_offset: after_offset.map(str::to_string),
                },
            )
            .await
    }

    /// Replay a run's durable events, optionally resuming after an offset.
    ///
    /// Only pass an offset that a previous `ObserveRun` produced; live `Send`
    /// offsets use different numbering and can skip events.
    pub async fn observe_run(
        &self,
        run_id: impl Into<String>,
        after_offset: Option<&str>,
    ) -> Result<Run> {
        let run_id = run_id.into();
        let stream = self.observe_run_stream(&run_id, after_offset).await?;
        Ok(Run::durable(self.clone(), String::new(), run_id, stream))
    }

    /// Block until a live run reaches a terminal status.
    pub async fn wait_live_run(&self, run_id: &str) -> Result<RunOutcome> {
        let response: proto::WaitLiveRunResponse = self
            .unary(
                AGENT_SERVICE,
                "WaitLiveRun",
                &proto::WaitLiveRunRequest {
                    run_id: run_id.to_string(),
                },
            )
            .await?;
        Ok(RunOutcome::from_result(response.result.unwrap_or_default()))
    }

    /// A point-in-time snapshot of a run.
    pub async fn get_run(&self, run_id: impl Into<String>) -> Result<RunOutcome> {
        let response: proto::GetRunResponse = self
            .unary(
                AGENT_SERVICE,
                "GetRun",
                &proto::GetRunRequest {
                    run_id: run_id.into(),
                    options: Some(proto::GetRunOptions {
                        runtime: RuntimeFilter::Auto.to_proto(),
                        cwd: String::new(),
                        agent_id: String::new(),
                        api_key: self.inner.api_key.clone().unwrap_or_default(),
                    }),
                },
            )
            .await?;
        Ok(RunOutcome::from_snapshot(response.run.unwrap_or_default()))
    }

    /// The raw conversation document for a run, as JSON.
    pub async fn run_conversation(&self, run_id: impl Into<String>) -> Result<JsonValue> {
        let response: proto::GetRunConversationResponse = self
            .unary(
                AGENT_SERVICE,
                "GetRunConversation",
                &proto::GetRunConversationRequest {
                    run_id: run_id.into(),
                },
            )
            .await?;
        serde_json::from_str(&response.conversation_json).map_err(|source| {
            Error::transport(format!(
                "the conversation document was not valid JSON: {source}"
            ))
        })
    }

    /// Request cancellation of an in-flight run.
    pub async fn cancel_run(&self, run_id: &str, agent_id: Option<&str>) -> Result<()> {
        let _: proto::CancelRunResponse = self
            .unary(
                AGENT_SERVICE,
                "CancelRun",
                &proto::CancelRunRequest {
                    run_id: run_id.to_string(),
                    agent_id: agent_id.map(str::to_string),
                },
            )
            .await?;
        Ok(())
    }

    // ---- one-liners -------------------------------------------------------

    /// Create an agent, send one prompt, wait for the answer, and close.
    ///
    /// The shortest path from nothing to an answer. The agent is closed even
    /// when the run fails; the bridge keeps running for reuse, so call
    /// [`Client::close`] when the program is finished with it.
    ///
    /// ```no_run
    /// # async fn demo() -> cursor_sdk::Result<()> {
    /// use cursor_sdk::{AgentOptions, Client};
    ///
    /// let client = Client::new();
    /// let answer = client
    ///     .prompt(AgentOptions::local(".").model("composer-2.5"), "What does this repo do?")
    ///     .await?;
    /// client.close().await?;
    /// # let _ = answer;
    /// # Ok(()) }
    /// ```
    pub async fn prompt(&self, options: AgentOptions, prompt: impl Into<Prompt>) -> Result<String> {
        let agent = self.create_agent(options).await?;
        let result = async {
            let run = agent.send(prompt).await?;
            run.text().await
        }
        .await;
        // Close regardless: a failed turn should not leak the agent's local
        // resources either.
        if let Err(error) = agent.close().await {
            tracing::debug!(target: "cursor_sdk", %error, "closing the prompt agent failed");
        }
        result
    }

    // ---- artifacts and usage ----------------------------------------------

    pub(crate) async fn list_artifacts(&self, agent_id: &str) -> Result<Vec<Artifact>> {
        let response: proto::ListArtifactsResponse = self
            .unary(
                AGENT_SERVICE,
                "ListArtifacts",
                &proto::ListArtifactsRequest {
                    agent_id: agent_id.to_string(),
                },
            )
            .await?;
        Ok(response.artifacts.into_iter().map(Artifact::from).collect())
    }

    pub(crate) async fn download_artifact_stream(
        &self,
        agent_id: &str,
        path: &str,
    ) -> Result<ServerStream<proto::DownloadArtifactChunk>> {
        self.connection()
            .await?
            .transport
            .server_stream(
                AGENT_SERVICE,
                "DownloadArtifact",
                &proto::DownloadArtifactRequest {
                    agent_id: agent_id.to_string(),
                    path: path.to_string(),
                },
            )
            .await
    }

    pub(crate) async fn usage(&self, agent_id: &str, run_id: Option<&str>) -> Result<AgentUsage> {
        let response: proto::GetUsageResponse = self
            .unary(
                AGENT_SERVICE,
                "GetUsage",
                &proto::GetUsageRequest {
                    agent_id: agent_id.to_string(),
                    run_id: run_id.map(str::to_string),
                },
            )
            .await?;
        Ok(response.usage.unwrap_or_default().into())
    }

    pub(crate) async fn list_runs(
        &self,
        agent_id: &str,
        filter: &ListRuns,
        cwd: Option<&Path>,
    ) -> Result<Page<RunOutcome>> {
        let response: proto::ListRunsResponse = self
            .unary(
                AGENT_SERVICE,
                "ListRuns",
                &proto::ListRunsRequest {
                    agent_id: agent_id.to_string(),
                    options: Some(filter.to_proto(cwd, self.inner.api_key.as_deref())),
                },
            )
            .await?;
        Ok(Page::new(
            response
                .items
                .into_iter()
                .map(RunOutcome::from_snapshot)
                .collect(),
            response.next_cursor,
        ))
    }

    pub(crate) async fn list_agent_messages(
        &self,
        agent_id: &str,
        filter: &ListMessages,
        cwd: Option<&Path>,
    ) -> Result<Vec<AgentMessage>> {
        let response: proto::ListAgentMessagesResponse = self
            .unary(
                AGENT_SERVICE,
                "ListAgentMessages",
                &proto::ListAgentMessagesRequest {
                    agent_id: agent_id.to_string(),
                    options: Some(filter.to_proto(cwd, self.inner.api_key.as_deref())),
                },
            )
            .await?;
        Ok(response
            .messages
            .into_iter()
            .map(AgentMessage::from)
            .collect())
    }

    pub(crate) async fn agent_lifecycle(
        &self,
        method: &str,
        agent_id: &str,
        cwd: Option<&Path>,
    ) -> Result<()> {
        let options = Some(self.agent_operation_options(cwd));
        match method {
            "ArchiveAgent" => {
                let _: proto::ArchiveAgentResponse = self
                    .unary(
                        AGENT_SERVICE,
                        method,
                        &proto::ArchiveAgentRequest {
                            agent_id: agent_id.to_string(),
                            options,
                        },
                    )
                    .await?;
            }
            "UnarchiveAgent" => {
                let _: proto::UnarchiveAgentResponse = self
                    .unary(
                        AGENT_SERVICE,
                        method,
                        &proto::UnarchiveAgentRequest {
                            agent_id: agent_id.to_string(),
                            options,
                        },
                    )
                    .await?;
            }
            "DeleteAgent" => {
                let _: proto::DeleteAgentResponse = self
                    .unary(
                        AGENT_SERVICE,
                        method,
                        &proto::DeleteAgentRequest {
                            agent_id: agent_id.to_string(),
                            options,
                        },
                    )
                    .await?;
            }
            "CloseAgent" => {
                let _: proto::CloseAgentResponse = self
                    .unary(
                        AGENT_SERVICE,
                        method,
                        &proto::CloseAgentRequest {
                            agent_id: agent_id.to_string(),
                        },
                    )
                    .await?;
            }
            "ReloadAgent" => {
                let _: proto::ReloadAgentResponse = self
                    .unary(
                        AGENT_SERVICE,
                        method,
                        &proto::ReloadAgentRequest {
                            agent_id: agent_id.to_string(),
                        },
                    )
                    .await?;
            }
            other => {
                return Err(Error::Config(format!(
                    "{other} is not an agent lifecycle RPC"
                )))
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_spawns_nothing_until_it_is_used() {
        let client = Client::builder().api_key("test-key").build();
        assert_eq!(client.api_key(), Some("test-key"));
        // No bridge, no panic, no process.
        assert!(format!("{client:?}").contains("endpoint: None"));
    }

    #[test]
    fn catalog_calls_require_a_key_on_the_request() {
        // An empty key is the same as none for a catalog call, which fails
        // closed rather than falling back to the bridge environment.
        let client = Client::builder().api_key("").build();
        let error = client.cursor_options().unwrap_err();
        assert!(error.to_string().contains("catalog calls require"));
    }

    #[tokio::test]
    async fn a_closed_client_refuses_further_work() {
        let client = Client::builder().api_key("k").build();
        client.close().await.unwrap();
        let error = client.ping().await.unwrap_err();
        assert!(error.to_string().contains("closed"));
    }

    #[tokio::test]
    async fn attaching_with_a_custom_store_is_rejected() {
        let client = Client::builder()
            .api_key("k")
            .endpoint("http://127.0.0.1:1", "token")
            .agent_store(crate::callback::store::MemoryStore::new())
            .build();
        let error = client.ping().await.unwrap_err();
        assert!(error.to_string().contains("custom agent store"));
    }

    #[test]
    fn list_filters_encode_what_was_set() {
        let encoded = ListAgents::new()
            .limit(10)
            .include_archived(true)
            .runtime(RuntimeFilter::Cloud)
            .to_proto(Some("key"));
        assert_eq!(encoded.limit, 10);
        assert_eq!(encoded.include_archived, Some(true));
        assert_eq!(encoded.runtime, proto::Runtime::Cloud as i32);
        assert_eq!(encoded.api_key, "key");

        let bare = ListAgents::new().to_proto(None);
        assert_eq!(bare.include_archived, None, "unset stays unset");
        assert_eq!(bare.runtime, proto::Runtime::Auto as i32);
    }
}
