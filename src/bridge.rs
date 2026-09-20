//! Finding, spawning, and stopping `cursor-sdk-bridge`.
//!
//! The lifecycle is the one in the bridge's `docs/protocol.md`: spawn the
//! executable with `CURSOR_API_KEY` in its environment, scan **stderr** for the
//! `cursor-sdk-bridge ready ` line, validate the JSON after it, read the bearer
//! token out of `authTokenFile`, and stop the process with
//! `SdkBridgeControlService.Shutdown` before escalating to a kill.
//!
//! A [`Client`](crate::Client) creates one of these lazily on first use, so a
//! program that never talks to an agent never pays for a bridge process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::{BridgeError, Error, Result};

/// The literal stderr prefix that precedes the discovery JSON. The trailing
/// space is part of it.
const READY_PREFIX: &str = "cursor-sdk-bridge ready ";
/// The discovery payload schema this crate understands.
const SUPPORTED_SCHEMA_VERSION: u32 = 1;
/// Executable name looked up on `PATH`.
const EXECUTABLE: &str = if cfg!(windows) {
    "cursor-sdk-bridge.exe"
} else {
    "cursor-sdk-bridge"
};
/// Keep the tail of stderr for error messages, bounded so a chatty bridge
/// cannot grow this without limit.
const MAX_CAPTURED_STDERR_LINES: usize = 200;

/// Where the bridge keeps durable local agent state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalStore {
    /// Bridge-owned `SQLite`. The bridge's default.
    Sqlite,
    /// Bridge-owned JSONL files under `root_dir`.
    Jsonl {
        /// Directory the JSONL files live in.
        root_dir: PathBuf,
    },
    /// Fully host-owned, served over `SdkStoreCallbackService`.
    ///
    /// Requires a store callback server; see
    /// [`AgentStore`](crate::AgentStore) and
    /// [`ClientBuilder::agent_store`](crate::ClientBuilder::agent_store). It
    /// can only be configured at launch, because agents may load state before
    /// any RPC arrives.
    Custom,
}

impl LocalStore {
    fn to_json(&self) -> String {
        match self {
            LocalStore::Sqlite => r#"{"type":"sqlite"}"#.to_string(),
            LocalStore::Jsonl { root_dir } => serde_json::json!({
                "type": "jsonl",
                "rootDir": root_dir.to_string_lossy(),
            })
            .to_string(),
            LocalStore::Custom => r#"{"type":"custom"}"#.to_string(),
        }
    }
}

/// A loopback callback endpoint the bridge should call back into.
#[derive(Debug, Clone)]
pub struct CallbackEndpoint {
    /// Connect base URL of the adapter-hosted server.
    pub url: String,
    /// Bearer token the bridge presents on every callback. Validate it.
    pub auth_token: String,
}

/// How to launch (or attach to) the bridge.
#[derive(Debug, Clone)]
pub struct BridgeOptions {
    /// Explicit path to the executable. Overrides discovery.
    pub binary: Option<PathBuf>,
    /// `--workspace`: the default `cwd` for local agents and store discovery.
    pub workspace: Option<PathBuf>,
    /// Placed in the bridge environment as `CURSOR_API_KEY`.
    ///
    /// This is *not* a substitute for setting the key on request options; see
    /// [`crate::ClientBuilder::api_key`].
    pub api_key: Option<String>,
    /// `--host`. Defaults to the bridge's own `127.0.0.1`.
    pub host: Option<String>,
    /// `--port`. Defaults to an ephemeral port read back from the ready line.
    pub port: Option<u16>,
    /// `--state-root`.
    pub state_root: Option<PathBuf>,
    /// `--local-store`.
    pub local_store: Option<LocalStore>,
    /// `--tool-callback-url` / `--tool-callback-auth-token`.
    pub tool_callback: Option<CallbackEndpoint>,
    /// `--store-callback-url` / `--store-callback-auth-token`. Launch-time only.
    pub store_callback: Option<CallbackEndpoint>,
    /// Extra command-line arguments, appended verbatim.
    pub extra_args: Vec<String>,
    /// Extra environment variables for the bridge process.
    pub env: HashMap<String, String>,
    /// `CURSOR_SDK_CLIENT_LANGUAGE`, so Cursor can attribute traffic.
    pub client_language: String,
    /// How long to wait for the ready line.
    pub startup_timeout: Duration,
    /// How long a graceful shutdown may take before the process is killed.
    pub shutdown_timeout: Duration,
    /// `Shutdown.grace_seconds`: how long the bridge may drain in-flight RPCs.
    ///
    /// Zero means exit immediately, which is what a closing client wants. A
    /// non-zero grace must stay below `shutdown_timeout`, or the wait expires
    /// while the bridge is still legitimately draining and it gets killed.
    pub shutdown_grace: Duration,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            binary: None,
            workspace: None,
            api_key: None,
            host: None,
            port: None,
            state_root: None,
            local_store: None,
            tool_callback: None,
            store_callback: None,
            extra_args: Vec::new(),
            env: HashMap::new(),
            client_language: "rust".to_string(),
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            shutdown_grace: Duration::ZERO,
        }
    }
}

/// What the bridge reported about itself during the handshake.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BridgeInfo {
    /// Bridge build version.
    pub server_version: Option<String>,
    /// Bridge process ID.
    pub pid: Option<u32>,
    /// The endpoint to connect to.
    pub url: String,
    /// The resolved workspace directory the bridge was launched for.
    pub workspace_ref: Option<String>,
    /// Directory holding bridge-owned durable agent state.
    pub state_root: Option<String>,
    /// Advertised agent concurrency limit, when configured.
    pub max_concurrent_agents: Option<u32>,
    /// Advertised maximum message size, when configured.
    pub max_message_bytes: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadyLine {
    schema_version: u32,
    #[serde(default)]
    server_version: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    transport: String,
    protocol: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    auth_token_file: Option<String>,
    /// Older bridges inline the token instead of writing a file.
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    workspace_ref: Option<String>,
    #[serde(default)]
    state_root: Option<String>,
    #[serde(default)]
    max_concurrent_agents: Option<u32>,
    #[serde(default)]
    max_message_bytes: Option<u64>,
}

/// A bridge this crate owns and must stop, or one it merely borrows.
#[derive(Debug)]
pub(crate) struct Bridge {
    endpoint: String,
    token: String,
    info: Option<BridgeInfo>,
    process: Option<Arc<ManagedProcess>>,
    shutdown_timeout: Duration,
}

#[derive(Debug)]
struct ManagedProcess {
    child: Mutex<Option<Child>>,
}

impl ManagedProcess {
    /// Kill the process without awaiting. Safe to call more than once.
    fn kill_now(&self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

/// Every managed bridge still alive in this process, for [`install_exit_guard`].
fn registry() -> &'static Mutex<Vec<Weak<ManagedProcess>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Weak<ManagedProcess>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

fn register(process: &Arc<ManagedProcess>) {
    if let Ok(mut entries) = registry().lock() {
        entries.retain(|entry| entry.strong_count() > 0);
        entries.push(Arc::downgrade(process));
    }
}

/// Kill every bridge this process spawned. Used by the exit guard.
fn kill_all_bridges() {
    if let Ok(entries) = registry().lock() {
        for entry in entries.iter() {
            if let Some(process) = entry.upgrade() {
                process.kill_now();
            }
        }
    }
}

/// Kill managed bridges when this program receives Ctrl-C (or `SIGTERM`).
///
/// [`Client::close`](crate::Client::close) and `Drop` already stop the bridge
/// on every normal path, including a panic unwind. A signal is the gap: the
/// process is torn down without running destructors, which would leave the
/// bridge running. Call this once, early, from a binary that wants that gap
/// closed. Libraries should leave the choice to the application, since this
/// installs a process-wide signal handler.
///
/// Requires a Tokio runtime. Calling it more than once is harmless.
pub fn install_exit_guard() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut terminate = match signal(SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not listen for SIGTERM");
                    let _ = tokio::signal::ctrl_c().await;
                    kill_all_bridges();
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::debug!("signal received; stopping managed cursor-sdk-bridge processes");
        kill_all_bridges();
    });
}

impl Bridge {
    /// Attach to a bridge somebody else is running.
    pub(crate) fn attach(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            info: None,
            process: None,
            shutdown_timeout: Duration::from_secs(5),
        }
    }

    /// Spawn a bridge and complete the ready-line handshake.
    pub(crate) async fn spawn(options: &BridgeOptions) -> Result<Self> {
        let binary = resolve_binary(options.binary.as_deref())?;

        // Catch a wrong-platform or wrong-contract binary here, where the
        // error can say so, rather than as an exec failure or a confusing
        // handshake error after the process is already running.
        crate::manifest::preflight(&binary).await?;

        let mut command = Command::new(&binary);
        command
            .args(build_args(options))
            .envs(&options.env)
            .env("CURSOR_SDK_CLIENT_LANGUAGE", &options.client_language)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Belt and braces alongside our own Drop: if this handle is
            // dropped without a stop(), Tokio kills the child too.
            .kill_on_drop(true);

        if let Some(api_key) = &options.api_key {
            command.env("CURSOR_API_KEY", api_key);
        }

        let mut child = command.spawn().map_err(|source| BridgeError::Spawn {
            path: binary.display().to_string(),
            source,
        })?;

        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child
            .stderr
            .take()
            .expect("stderr was piped when the bridge was spawned");

        // The bridge blocks if either pipe fills, so both are drained forever.
        if let Some(stdout) = stdout {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "cursor_sdk::bridge", "stdout: {line}");
                }
            });
        }

        let mut lines = BufReader::new(stderr).lines();
        let ready = match tokio::time::timeout(options.startup_timeout, read_ready_line(&mut lines))
            .await
        {
            Ok(Ok(ready)) => ready,
            Ok(Err(captured)) => {
                let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await
                {
                    Ok(Ok(status)) => status.to_string(),
                    _ => "still running with stderr closed".to_string(),
                };
                return Err(BridgeError::ExitedBeforeReady {
                    status,
                    stderr: captured,
                }
                .into());
            }
            Err(_) => {
                let _ = child.start_kill();
                return Err(BridgeError::StartupTimeout {
                    timeout: options.startup_timeout,
                    stderr: "(not captured: the ready line never arrived)".to_string(),
                }
                .into());
            }
        };

        // Keep draining stderr for the life of the process: a full pipe blocks
        // the bridge. The ready line is already consumed, so nothing here can
        // leak the bearer token that older bridges inline into it.
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.starts_with(READY_PREFIX) {
                    continue;
                }
                tracing::debug!(target: "cursor_sdk::bridge", "{line}");
            }
        });

        let (endpoint, token, info) = interpret_ready_line(&ready, pid).await?;

        let process = Arc::new(ManagedProcess {
            child: Mutex::new(Some(child)),
        });
        register(&process);

        tracing::debug!(
            target: "cursor_sdk::bridge",
            endpoint = %endpoint,
            pid = ?pid,
            "bridge ready"
        );

        Ok(Self {
            endpoint,
            token,
            info: Some(info),
            process: Some(process),
            shutdown_timeout: options.shutdown_timeout,
        })
    }

    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn info(&self) -> Option<&BridgeInfo> {
        self.info.as_ref()
    }

    /// Whether this crate owns the process and is responsible for stopping it.
    pub(crate) fn is_managed(&self) -> bool {
        self.process.is_some()
    }

    /// Wait for the process to exit, then kill it if it overstays.
    ///
    /// The graceful `Shutdown` RPC is issued by the caller, which owns the
    /// transport; this half handles the waiting and the escalation.
    pub(crate) async fn wait_for_exit(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        let child = process.child.lock().ok().and_then(|mut guard| guard.take());
        let Some(mut child) = child else {
            return;
        };

        match tokio::time::timeout(self.shutdown_timeout, child.wait()).await {
            Ok(Ok(status)) => {
                tracing::debug!(target: "cursor_sdk::bridge", "bridge exited: {status}");
            }
            Ok(Err(error)) => {
                tracing::warn!(target: "cursor_sdk::bridge", %error, "waiting for the bridge failed");
                let _ = child.kill().await;
            }
            Err(_) => {
                tracing::warn!(
                    target: "cursor_sdk::bridge",
                    "the bridge did not exit within {:?}; killing it",
                    self.shutdown_timeout
                );
                let _ = child.kill().await;
            }
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // A Client that was dropped instead of closed (including on a panic
        // unwind) must not leave a bridge behind. Drop cannot await, so this
        // is the hard stop; Client::close does the graceful one first.
        if let Some(process) = &self.process {
            process.kill_now();
        }
    }
}

/// Read stderr until the ready line arrives.
///
/// `Err` carries the captured stderr tail, for the "exited before ready" error.
async fn read_ready_line<R>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
) -> std::result::Result<String, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut captured: Vec<String> = Vec::new();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Some(payload) = line.strip_prefix(READY_PREFIX) {
                    return Ok(payload.to_string());
                }
                // Ordinary diagnostics: forward them, and keep scanning.
                tracing::debug!(target: "cursor_sdk::bridge", "{line}");
                captured.push(line);
                if captured.len() > MAX_CAPTURED_STDERR_LINES {
                    captured.remove(0);
                }
            }
            Ok(None) => return Err(captured.join("\n")),
            Err(error) => {
                captured.push(format!("(reading stderr failed: {error})"));
                return Err(captured.join("\n"));
            }
        }
    }
}

/// Validate the discovery JSON and resolve the endpoint and bearer token.
///
/// The raw line is never included in an error: older bridges inline the token.
async fn interpret_ready_line(
    payload: &str,
    pid: Option<u32>,
) -> Result<(String, String, BridgeInfo)> {
    let ready: ReadyLine = serde_json::from_str(payload).map_err(|source| {
        BridgeError::Handshake(format!("the discovery JSON could not be parsed: {source}"))
    })?;

    if ready.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(BridgeError::Handshake(format!(
            "unsupported discovery schemaVersion {} (this SDK speaks {SUPPORTED_SCHEMA_VERSION}); \
             upgrade the SDK or pin an older bridge",
            ready.schema_version
        ))
        .into());
    }
    if ready.transport != "tcp" {
        return Err(BridgeError::Handshake(format!(
            "unsupported bridge transport {:?}; this SDK speaks \"tcp\"",
            ready.transport
        ))
        .into());
    }
    if ready.protocol != "connect" {
        return Err(BridgeError::Handshake(format!(
            "unsupported bridge protocol {:?}; this SDK speaks \"connect\"",
            ready.protocol
        ))
        .into());
    }

    let endpoint = match (&ready.url, &ready.host, ready.port) {
        (Some(url), _, _) if !url.is_empty() => url.trim_end_matches('/').to_string(),
        (_, Some(host), Some(port)) => {
            // IPv6 literals need brackets in an authority.
            if host.contains(':') && !host.starts_with('[') {
                format!("http://[{host}]:{port}")
            } else {
                format!("http://{host}:{port}")
            }
        }
        _ => {
            return Err(BridgeError::Handshake(
                "the discovery payload carried neither a url nor a host and port".to_string(),
            )
            .into())
        }
    };

    // Prefer the inline token when an older bridge supplies one, exactly as
    // docs/protocol.md prescribes.
    let token = match ready.auth_token.filter(|token| !token.is_empty()) {
        Some(token) => token,
        None => {
            let path = ready.auth_token_file.clone().ok_or_else(|| {
                BridgeError::Handshake(
                    "the discovery payload carried neither authToken nor authTokenFile".to_string(),
                )
            })?;
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|source| BridgeError::AuthToken {
                    path: path.clone(),
                    source,
                })?
                .trim()
                .to_string()
        }
    };

    if token.is_empty() {
        return Err(BridgeError::Handshake("the bridge auth token was empty".to_string()).into());
    }

    let info = BridgeInfo {
        server_version: ready.server_version,
        pid: ready.pid.or(pid),
        url: endpoint.clone(),
        workspace_ref: ready.workspace_ref,
        state_root: ready.state_root,
        max_concurrent_agents: ready.max_concurrent_agents,
        max_message_bytes: ready.max_message_bytes,
    };

    Ok((endpoint, token, info))
}

fn build_args(options: &BridgeOptions) -> Vec<String> {
    let mut args = Vec::new();
    let mut push = |flag: &str, value: String| {
        args.push(flag.to_string());
        args.push(value);
    };

    if let Some(workspace) = &options.workspace {
        push("--workspace", workspace.to_string_lossy().into_owned());
    }
    if let Some(host) = &options.host {
        push("--host", host.clone());
    }
    if let Some(port) = options.port {
        push("--port", port.to_string());
    }
    if let Some(state_root) = &options.state_root {
        push("--state-root", state_root.to_string_lossy().into_owned());
    }
    if let Some(store) = &options.local_store {
        push("--local-store", store.to_json());
    }
    if let Some(callback) = &options.tool_callback {
        push("--tool-callback-url", callback.url.clone());
        push("--tool-callback-auth-token", callback.auth_token.clone());
    }
    if let Some(callback) = &options.store_callback {
        push("--store-callback-url", callback.url.clone());
        push("--store-callback-auth-token", callback.auth_token.clone());
    }
    args.extend(options.extra_args.iter().cloned());
    args
}

/// Locate the executable: explicit path, then `CURSOR_SDK_BRIDGE_BIN`, then
/// `PATH` (where `pip install cursor-sdk` puts it), then the per-user Cursor
/// directory.
fn resolve_binary(explicit: Option<&Path>) -> Result<PathBuf> {
    // An override that does not exist is a misconfiguration, not a reason to
    // silently run some other binary: falling through could launch a different
    // bridge version than the caller asked for.
    for (path, source) in [
        (
            explicit.map(Path::to_path_buf),
            "Client::builder().bridge_binary(..)",
        ),
        (
            std::env::var_os("CURSOR_SDK_BRIDGE_BIN").map(PathBuf::from),
            "CURSOR_SDK_BRIDGE_BIN",
        ),
    ] {
        let Some(path) = path else { continue };
        if path.is_file() {
            return Ok(path);
        }
        return Err(BridgeError::NotFound(format!(
            "{source} points at {}, which is not a file",
            path.display()
        ))
        .into());
    }

    if let Some(path) = find_on_path() {
        return Ok(path);
    }

    let mut tried = vec![format!("{EXECUTABLE} on PATH")];
    if let Some(home) = home_dir() {
        let path = home
            .join(".cursor")
            .join("sdk-bridge")
            .join("bin")
            .join(EXECUTABLE);
        if path.is_file() {
            return Ok(path);
        }
        tried.push(path.display().to_string());
    }

    Err(BridgeError::NotFound(format!("tried {}", tried.join(", "))).into())
}

fn find_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(EXECUTABLE))
        .find(|candidate| candidate.is_file())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Surfaced so callers can describe a misconfiguration without re-deriving it.
impl BridgeOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        // The bridge rejects a half-configured callback pair at startup; catch
        // it here so the error names the builder method instead of a CLI flag.
        for (name, endpoint) in [
            ("tool_callback", &self.tool_callback),
            ("store_callback", &self.store_callback),
        ] {
            if let Some(endpoint) = endpoint {
                if endpoint.url.is_empty() || endpoint.auth_token.is_empty() {
                    return Err(Error::Config(format!(
                        "{name} needs both a url and an auth token"
                    )));
                }
            }
        }
        if self.shutdown_grace >= self.shutdown_timeout && !self.shutdown_grace.is_zero() {
            return Err(Error::Config(format!(
                "shutdown_grace ({:?}) must be shorter than shutdown_timeout ({:?}), or the \
                 bridge gets killed while it is still draining",
                self.shutdown_grace, self.shutdown_timeout
            )));
        }
        if matches!(self.local_store, Some(LocalStore::Custom)) && self.store_callback.is_none() {
            return Err(Error::Config(
                "a custom local store needs a store callback server; the bridge can only be told \
                 about one at launch"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_a_ready_line() {
        let payload = r#"{"schemaVersion":1,"serverVersion":"1.0.0","pid":123,"transport":"tcp",
            "protocol":"connect","host":"127.0.0.1","port":49152,
            "url":"http://127.0.0.1:49152","authToken":"tok_abc",
            "workspaceRef":"/repo","stateRoot":"/state","unknownFutureField":true}"#;
        let (endpoint, token, info) = interpret_ready_line(payload, Some(123)).await.unwrap();
        assert_eq!(endpoint, "http://127.0.0.1:49152");
        assert_eq!(token, "tok_abc");
        assert_eq!(info.workspace_ref.as_deref(), Some("/repo"));
        assert_eq!(info.pid, Some(123));
    }

    #[tokio::test]
    async fn reads_the_token_from_its_file() {
        let directory =
            std::env::temp_dir().join(format!("cursor-sdk-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let token_file = directory.join("auth-token");
        tokio::fs::write(&token_file, "  tok_from_file\n")
            .await
            .unwrap();

        let payload = serde_json::json!({
            "schemaVersion": 1, "transport": "tcp", "protocol": "connect",
            "url": "http://127.0.0.1:5000/",
            "authTokenFile": token_file.to_string_lossy(),
        })
        .to_string();

        let (endpoint, token, _) = interpret_ready_line(&payload, None).await.unwrap();
        assert_eq!(token, "tok_from_file", "surrounding whitespace is trimmed");
        assert_eq!(
            endpoint, "http://127.0.0.1:5000",
            "the trailing slash is dropped"
        );
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_an_unknown_schema_version() {
        let payload = r#"{"schemaVersion":2,"transport":"tcp","protocol":"connect",
            "url":"http://127.0.0.1:1","authToken":"t"}"#;
        let error = interpret_ready_line(payload, None).await.unwrap_err();
        assert!(matches!(error, Error::Bridge(BridgeError::Handshake(_))));
        assert!(error.to_string().contains("schemaVersion 2"));
    }

    #[tokio::test]
    async fn rejects_a_non_connect_protocol() {
        let payload = r#"{"schemaVersion":1,"transport":"tcp","protocol":"grpc",
            "url":"http://127.0.0.1:1","authToken":"t"}"#;
        let error = interpret_ready_line(payload, None).await.unwrap_err();
        assert!(error.to_string().contains("protocol"));
    }

    #[tokio::test]
    async fn brackets_ipv6_hosts() {
        let payload = r#"{"schemaVersion":1,"transport":"tcp","protocol":"connect",
            "host":"::1","port":8080,"authToken":"t"}"#;
        let (endpoint, _, _) = interpret_ready_line(payload, None).await.unwrap();
        assert_eq!(endpoint, "http://[::1]:8080");
    }

    #[tokio::test]
    async fn exited_before_ready_keeps_stderr() {
        let mut lines = BufReader::new(&b"boot\nbad flag --nope\n"[..]).lines();
        let captured = read_ready_line(&mut lines).await.unwrap_err();
        assert_eq!(captured, "boot\nbad flag --nope");
    }

    #[tokio::test]
    async fn finds_the_ready_line_after_diagnostics() {
        let input = concat!(
            "starting up\n",
            "cursor-sdk-bridge ready {\"schemaVersion\":1}\n",
        );
        let mut lines = BufReader::new(input.as_bytes()).lines();
        let payload = read_ready_line(&mut lines).await.unwrap();
        assert_eq!(payload, "{\"schemaVersion\":1}");
    }

    #[test]
    fn an_explicit_binary_that_is_missing_does_not_fall_back() {
        // Falling through to PATH here could launch a different bridge version
        // than the caller asked for.
        let error = resolve_binary(Some(Path::new("/definitely/not/here/bridge"))).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("bridge_binary"), "{text}");
        assert!(text.contains("/definitely/not/here/bridge"), "{text}");
    }

    #[test]
    fn builds_launch_arguments() {
        let options = BridgeOptions {
            workspace: Some(PathBuf::from("/repo")),
            port: Some(7000),
            local_store: Some(LocalStore::Jsonl {
                root_dir: PathBuf::from("/state"),
            }),
            tool_callback: Some(CallbackEndpoint {
                url: "http://127.0.0.1:9000".into(),
                auth_token: "cb".into(),
            }),
            extra_args: vec!["--verbose".into()],
            ..Default::default()
        };
        let args = build_args(&options);
        assert_eq!(
            args,
            vec![
                "--workspace",
                "/repo",
                "--port",
                "7000",
                "--local-store",
                r#"{"rootDir":"/state","type":"jsonl"}"#,
                "--tool-callback-url",
                "http://127.0.0.1:9000",
                "--tool-callback-auth-token",
                "cb",
                "--verbose",
            ]
        );
    }

    #[test]
    fn a_grace_longer_than_the_shutdown_timeout_is_rejected() {
        let options = BridgeOptions {
            shutdown_grace: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        assert!(matches!(options.validate(), Err(Error::Config(_))));
    }

    #[test]
    fn custom_store_without_a_callback_server_is_rejected() {
        let options = BridgeOptions {
            local_store: Some(LocalStore::Custom),
            ..Default::default()
        };
        assert!(matches!(options.validate(), Err(Error::Config(_))));
    }
}
