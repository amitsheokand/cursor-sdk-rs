//! Builders for agent and send options.
//!
//! These mirror `AgentOptions` / `SendOptions` from the contract, but in
//! ordinary Rust: JSON Schemas are [`serde_json::Value`]s, enumerations are
//! real enums, and the presence-sensitive fields are `Option`s so "unset" and
//! "empty" stay distinguishable the way the proto intends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::bridge::LocalStore;
use crate::error::{Error, Result};
use crate::json::json_to_struct;
use crate::proto;
use crate::types::{CloudEnvironment, CloudEnvironmentKind, ModelChoice};

/// Conversation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// Standard agent mode: the agent edits as it goes.
    #[default]
    Agent,
    /// Plan mode: produce a plan before (or instead of) making edits.
    Plan,
}

impl AgentMode {
    fn to_proto(self) -> proto::AgentModeOption {
        match self {
            AgentMode::Agent => proto::AgentModeOption::Agent,
            AgentMode::Plan => proto::AgentModeOption::Plan,
        }
    }
}

/// A source of Cursor settings a local agent should honor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingSource {
    /// `.cursor` settings in the project.
    Project,
    /// The user's own settings.
    User,
    /// Team settings.
    Team,
    /// MDM-managed settings.
    Mdm,
    /// Settings contributed by plugins.
    Plugins,
    /// Every available source.
    All,
}

impl SettingSource {
    fn to_proto(self) -> proto::SettingSource {
        match self {
            SettingSource::Project => proto::SettingSource::Project,
            SettingSource::User => proto::SettingSource::User,
            SettingSource::Team => proto::SettingSource::Team,
            SettingSource::Mdm => proto::SettingSource::Mdm,
            SettingSource::Plugins => proto::SettingSource::Plugins,
            SettingSource::All => proto::SettingSource::All,
        }
    }
}

/// Transport for an HTTP-style MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpTransport {
    /// Streamable HTTP.
    #[default]
    Http,
    /// Server-sent events.
    Sse,
}

/// OAuth client credentials for an HTTP MCP server.
#[derive(Debug, Clone, Default)]
pub struct McpAuth {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
}

/// An MCP server to expose to the agent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum McpServer {
    /// A subprocess speaking MCP over stdio.
    Stdio {
        /// Executable to run.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Extra environment variables.
        env: BTreeMap<String, String>,
        /// Working directory.
        cwd: Option<PathBuf>,
    },
    /// A remote MCP server reached over HTTP.
    Http {
        /// Transport flavor.
        transport: McpTransport,
        /// Server URL.
        url: String,
        /// Extra request headers.
        headers: BTreeMap<String, String>,
        /// OAuth credentials, when the server needs them.
        auth: Option<McpAuth>,
    },
}

impl McpServer {
    /// A stdio MCP server with no extra environment or working directory.
    pub fn stdio(
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        McpServer::Stdio {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    /// A streamable-HTTP MCP server.
    pub fn http(url: impl Into<String>) -> Self {
        McpServer::Http {
            transport: McpTransport::Http,
            url: url.into(),
            headers: BTreeMap::new(),
            auth: None,
        }
    }

    fn to_proto(&self) -> proto::McpServerConfig {
        let config = match self {
            McpServer::Stdio {
                command,
                args,
                env,
                cwd,
            } => proto::mcp_server_config::Config::Stdio(proto::StdioMcpServerConfig {
                command: command.clone(),
                args: args.clone(),
                env: env.clone().into_iter().collect(),
                cwd: cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }),
            McpServer::Http {
                transport,
                url,
                headers,
                auth,
            } => proto::mcp_server_config::Config::Http(proto::HttpMcpServerConfig {
                r#type: match transport {
                    McpTransport::Http => proto::HttpMcpTransportType::Http as i32,
                    McpTransport::Sse => proto::HttpMcpTransportType::Sse as i32,
                },
                url: url.clone(),
                headers: headers.clone().into_iter().collect(),
                auth: auth.as_ref().map(|auth| proto::McpAuthConfig {
                    client_id: auth.client_id.clone(),
                    client_secret: auth.client_secret.clone(),
                    scopes: auth.scopes.clone(),
                }),
            }),
        };
        proto::McpServerConfig {
            config: Some(config),
        }
    }
}

/// An MCP server a sub-agent may use.
#[derive(Debug, Clone)]
pub enum SubAgentMcpServer {
    /// The name of a server already configured on the parent agent.
    Named(String),
    /// A server defined inline for this sub-agent.
    Inline(Box<McpServer>),
}

/// A named sub-agent the main agent can delegate to.
#[derive(Debug, Clone, Default)]
pub struct SubAgent {
    /// When the main agent should delegate to this sub-agent.
    pub description: String,
    /// The sub-agent's system prompt.
    pub prompt: String,
    /// Model override. Ignored when `inherit_model` is true.
    pub model: Option<ModelChoice>,
    /// Inherit the parent's model instead of using `model`.
    pub inherit_model: Option<bool>,
    /// MCP servers available to the sub-agent.
    pub mcp_servers: Vec<SubAgentMcpServer>,
}

impl SubAgent {
    /// A sub-agent with a description and a prompt.
    pub fn new(description: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            prompt: prompt.into(),
            ..Default::default()
        }
    }

    fn to_proto(&self) -> proto::AgentDefinition {
        proto::AgentDefinition {
            description: self.description.clone(),
            prompt: self.prompt.clone(),
            model: self.model.as_ref().map(ModelChoice::to_proto),
            inherit_model: self.inherit_model,
            mcp_servers: self
                .mcp_servers
                .iter()
                .map(|server| proto::AgentDefinitionMcpServer {
                    value: Some(match server {
                        SubAgentMcpServer::Named(name) => {
                            proto::agent_definition_mcp_server::Value::Name(name.clone())
                        }
                        SubAgentMcpServer::Inline(config) => {
                            proto::agent_definition_mcp_server::Value::InlineConfig(
                                config.to_proto(),
                            )
                        }
                    }),
                })
                .collect(),
        }
    }
}

/// The declaration half of a custom tool: what the model sees.
///
/// The execution half is a handler registered with
/// [`Client::register_tool`](crate::Client::register_tool).
#[derive(Debug, Clone)]
pub struct CustomTool {
    /// Tool name, as the model will call it.
    pub name: String,
    /// What the tool does, for the model.
    pub description: Option<String>,
    /// JSON Schema object describing the arguments.
    pub input_schema: JsonValue,
    /// JSON Schema object describing the structured result, when declared.
    pub output_schema: Option<JsonValue>,
}

impl CustomTool {
    /// A tool with a description and an input schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: JsonValue,
    ) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema,
            output_schema: None,
        }
    }

    /// Declare the structured output schema (`Tool.outputSchema` in MCP).
    #[must_use]
    pub fn with_output_schema(mut self, schema: JsonValue) -> Self {
        self.output_schema = Some(schema);
        self
    }

    pub(crate) fn to_proto(&self) -> Result<proto::CustomToolDefinition> {
        let input_schema = json_to_struct(&self.input_schema).ok_or_else(|| {
            Error::Config(format!(
                "the input schema for custom tool {:?} must be a JSON object",
                self.name
            ))
        })?;
        let output_schema = match &self.output_schema {
            Some(schema) => Some(json_to_struct(schema).ok_or_else(|| {
                Error::Config(format!(
                    "the output schema for custom tool {:?} must be a JSON object",
                    self.name
                ))
            })?),
            None => None,
        };
        Ok(proto::CustomToolDefinition {
            description: self.description.clone(),
            input_schema: Some(input_schema),
            output_schema,
        })
    }
}

/// A repository a cloud agent works against.
#[derive(Debug, Clone, Default)]
pub struct CloudRepository {
    /// Git remote URL.
    pub url: String,
    /// Ref to start from: a branch, tag, or commit SHA.
    pub starting_ref: Option<String>,
    /// Pull request to associate the work with.
    pub pr_url: Option<String>,
}

impl CloudRepository {
    /// A repository at its default branch.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Start from a specific branch, tag, or SHA.
    #[must_use]
    pub fn starting_ref(mut self, value: impl Into<String>) -> Self {
        self.starting_ref = Some(value.into());
        self
    }
}

/// Options specific to a local agent (one that runs beside the bridge).
#[derive(Debug, Clone, Default)]
pub struct LocalAgent {
    cwd: Option<PathBuf>,
    dirs: Vec<PathBuf>,
    setting_sources: Vec<SettingSource>,
    sandbox: Option<bool>,
    store: Option<LocalStore>,
    auto_review: Option<bool>,
    custom_tools: BTreeMap<String, CustomTool>,
}

impl LocalAgent {
    /// A local agent working in `cwd`.
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: Some(cwd.as_ref().to_path_buf()),
            ..Default::default()
        }
    }

    /// Additional workspace roots, for a multi-root workspace.
    ///
    /// Project skills, rules, and workspace metadata then cover every entry.
    #[must_use]
    pub fn dirs(mut self, dirs: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        self.dirs = dirs
            .into_iter()
            .map(|dir| dir.as_ref().to_path_buf())
            .collect();
        self
    }

    /// Which Cursor setting sources the agent should read.
    #[must_use]
    pub fn setting_sources(mut self, sources: impl IntoIterator<Item = SettingSource>) -> Self {
        self.setting_sources = sources.into_iter().collect();
        self
    }

    /// Run the agent's commands inside Cursor's sandbox.
    #[must_use]
    pub fn sandbox(mut self, enabled: bool) -> Self {
        self.sandbox = Some(enabled);
        self
    }

    /// Where the bridge keeps this agent's durable state.
    ///
    /// [`LocalStore::Custom`] additionally needs a store callback server,
    /// configured on the client before the bridge launches.
    #[must_use]
    pub fn store(mut self, store: LocalStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Use classifier-backed automatic review for tool calls.
    #[must_use]
    pub fn auto_review(mut self, enabled: bool) -> Self {
        self.auto_review = Some(enabled);
        self
    }

    /// Declare a custom tool for this agent specifically.
    ///
    /// Tools registered on the client with
    /// [`Client::register_tool`](crate::Client::register_tool) are added
    /// automatically; this is for declaring one whose handler is registered
    /// separately.
    #[must_use]
    pub fn custom_tool(mut self, tool: CustomTool) -> Self {
        self.custom_tools.insert(tool.name.clone(), tool);
        self
    }

    pub(crate) fn merge_tools(&mut self, tools: impl IntoIterator<Item = CustomTool>) {
        for tool in tools {
            // An agent-level declaration wins over the client-wide one.
            self.custom_tools.entry(tool.name.clone()).or_insert(tool);
        }
    }

    pub(crate) fn uses_custom_store(&self) -> bool {
        matches!(self.store, Some(LocalStore::Custom))
    }

    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    fn to_proto(&self) -> Result<proto::LocalAgentOptions> {
        let mut custom_tools = std::collections::HashMap::new();
        for (name, tool) in &self.custom_tools {
            custom_tools.insert(name.clone(), tool.to_proto()?);
        }
        Ok(proto::LocalAgentOptions {
            // The field is repeated for historical reasons; send at most one
            // entry and use `dirs` for multi-root workspaces.
            cwd: self
                .cwd
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            setting_sources: self
                .setting_sources
                .iter()
                .map(|source| source.to_proto() as i32)
                .collect(),
            sandbox_options: self.sandbox.map(|enabled| proto::SandboxOptions {
                enabled: Some(enabled),
            }),
            store: self.store.as_ref().map(|store| match store {
                LocalStore::Sqlite => proto::LocalAgentStoreConfig {
                    r#type: "sqlite".into(),
                    root_dir: String::new(),
                },
                LocalStore::Jsonl { root_dir } => proto::LocalAgentStoreConfig {
                    r#type: "jsonl".into(),
                    root_dir: root_dir.to_string_lossy().into_owned(),
                },
                LocalStore::Custom => proto::LocalAgentStoreConfig {
                    r#type: "custom".into(),
                    root_dir: String::new(),
                },
            }),
            auto_review: self.auto_review,
            custom_tools,
            dirs: self
                .dirs
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        })
    }
}

/// Options specific to a cloud agent.
#[derive(Debug, Clone, Default)]
pub struct CloudAgent {
    env: Option<CloudEnvironment>,
    repos: Vec<CloudRepository>,
    work_on_current_branch: Option<bool>,
    auto_create_pr: Option<bool>,
    skip_reviewer_request: Option<bool>,
    env_vars: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
    open_as_cursor_github_app: Option<bool>,
}

impl CloudAgent {
    /// A cloud agent working on one repository.
    pub fn new(repository: impl Into<String>) -> Self {
        Self {
            repos: vec![CloudRepository::new(repository)],
            ..Default::default()
        }
    }

    /// A cloud agent with an empty workspace and no repository.
    ///
    /// No-repo agents have to be enabled for the account or team, and a
    /// repository-scoped API key cannot create one.
    pub fn no_repository() -> Self {
        Self::default()
    }

    /// Work on several repositories.
    #[must_use]
    pub fn repositories(mut self, repos: impl IntoIterator<Item = CloudRepository>) -> Self {
        self.repos = repos.into_iter().collect();
        self
    }

    /// Run on a self-hosted pool or a named machine instead of Cursor's cloud.
    #[must_use]
    pub fn environment(mut self, kind: CloudEnvironmentKind, name: impl Into<String>) -> Self {
        self.env = Some(CloudEnvironment {
            kind,
            name: name.into(),
        });
        self
    }

    /// Commit onto the starting branch instead of a new one.
    #[must_use]
    pub fn work_on_current_branch(mut self, enabled: bool) -> Self {
        self.work_on_current_branch = Some(enabled);
        self
    }

    /// Open a pull request when the agent finishes.
    #[must_use]
    pub fn auto_create_pr(mut self, enabled: bool) -> Self {
        self.auto_create_pr = Some(enabled);
        self
    }

    /// Do not request a reviewer on the pull request.
    #[must_use]
    pub fn skip_reviewer_request(mut self, enabled: bool) -> Self {
        self.skip_reviewer_request = Some(enabled);
        self
    }

    /// Environment variables for the agent's whole lifetime.
    #[must_use]
    pub fn env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_vars.insert(key.into(), value.into());
        self
    }

    /// A caller-owned tag, readable later on [`AgentRuntime::Cloud`].
    ///
    /// [`AgentRuntime::Cloud`]: crate::types::AgentRuntime::Cloud
    #[must_use]
    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Open pull requests as the Cursor GitHub App rather than the key's owner.
    #[must_use]
    pub fn open_as_cursor_github_app(mut self, enabled: bool) -> Self {
        self.open_as_cursor_github_app = Some(enabled);
        self
    }

    fn to_proto(&self) -> proto::CloudAgentOptions {
        proto::CloudAgentOptions {
            env: self.env.as_ref().map(|env| proto::CloudEnvironment {
                r#type: env.kind.to_proto() as i32,
                name: env.name.clone(),
            }),
            repos: self
                .repos
                .iter()
                .map(|repo| proto::CloudRepository {
                    url: repo.url.clone(),
                    starting_ref: repo.starting_ref.clone().unwrap_or_default(),
                    pr_url: repo.pr_url.clone().unwrap_or_default(),
                })
                .collect(),
            work_on_current_branch: self.work_on_current_branch,
            auto_create_pr: self.auto_create_pr,
            skip_reviewer_request: self.skip_reviewer_request,
            env_vars: self.env_vars.clone().into_iter().collect(),
            metadata: self.metadata.clone().into_iter().collect(),
            open_as_cursor_github_app: self.open_as_cursor_github_app,
        }
    }
}

/// Which runtime an agent uses.
#[derive(Debug, Clone)]
enum Runtime {
    Local(LocalAgent),
    Cloud(Box<CloudAgent>),
}

/// How to create or resume an agent.
///
/// A local agent needs a working directory and an explicit model:
///
/// ```
/// use cursor_sdk::{AgentOptions, LocalAgent, SettingSource};
///
/// // The short form.
/// let options = AgentOptions::local("/path/to/repo").model("composer-2.5");
///
/// // The same thing with the local runtime configured in detail.
/// let options = AgentOptions::new()
///     .model("composer-2.5")
///     .local_options(
///         LocalAgent::new("/path/to/repo")
///             .setting_sources([SettingSource::Project])
///             .sandbox(true),
///     );
/// # let _ = options;
/// ```
#[derive(Debug, Clone, Default)]
pub struct AgentOptions {
    pub(crate) model: Option<ModelChoice>,
    pub(crate) api_key: Option<String>,
    name: Option<String>,
    runtime: Option<Runtime>,
    mcp_servers: BTreeMap<String, McpServer>,
    sub_agents: BTreeMap<String, SubAgent>,
    agent_id: Option<String>,
    mode: Option<AgentMode>,
    tools: Option<Vec<String>>,
    disallowed_tools: Vec<String>,
}

impl AgentOptions {
    /// An empty set of options. Prefer [`AgentOptions::local`] or
    /// [`AgentOptions::cloud`], which pick a runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// A local agent rooted at `cwd`.
    pub fn local(cwd: impl AsRef<Path>) -> Self {
        Self::new().local_options(LocalAgent::new(cwd))
    }

    /// A cloud agent working on `repository`.
    pub fn cloud(repository: impl Into<String>) -> Self {
        Self::new().cloud_options(CloudAgent::new(repository))
    }

    /// Use a fully configured [`LocalAgent`].
    #[must_use]
    pub fn local_options(mut self, local: LocalAgent) -> Self {
        self.runtime = Some(Runtime::Local(local));
        self
    }

    /// Use a fully configured [`CloudAgent`].
    #[must_use]
    pub fn cloud_options(mut self, cloud: CloudAgent) -> Self {
        self.runtime = Some(Runtime::Cloud(Box::new(cloud)));
        self
    }

    /// The model to run. Required for local agents.
    ///
    /// Discover ids with [`Client::models`](crate::Client::models).
    #[must_use]
    pub fn model(mut self, model: impl Into<ModelChoice>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the client's Cursor API key for this agent.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// A display name for the agent.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Create the agent with a specific id.
    #[must_use]
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Start the agent in plan mode or agent mode.
    #[must_use]
    pub fn mode(mut self, mode: AgentMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Expose an MCP server to the agent.
    #[must_use]
    pub fn mcp_server(mut self, name: impl Into<String>, server: McpServer) -> Self {
        self.mcp_servers.insert(name.into(), server);
        self
    }

    /// Define a sub-agent the main agent can delegate to.
    #[must_use]
    pub fn sub_agent(mut self, name: impl Into<String>, definition: SubAgent) -> Self {
        self.sub_agents.insert(name.into(), definition);
        self
    }

    /// Offer the model only these built-in tools.
    ///
    /// An empty list means no built-in tools at all, which is different from
    /// leaving this unset (the default toolset). Local agents only.
    #[must_use]
    pub fn tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Remove built-in tools from the model's toolset. Deny wins over
    /// [`AgentOptions::tools`]. Local agents only.
    #[must_use]
    pub fn disallowed_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.disallowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// The local runtime options, when this is a local agent.
    pub(crate) fn local_mut(&mut self) -> Option<&mut LocalAgent> {
        match &mut self.runtime {
            Some(Runtime::Local(local)) => Some(local),
            _ => None,
        }
    }

    /// The working directory, for RPCs that route by `cwd`.
    pub(crate) fn cwd(&self) -> Option<PathBuf> {
        match &self.runtime {
            Some(Runtime::Local(local)) => local.cwd().map(Path::to_path_buf),
            _ => None,
        }
    }

    pub(crate) fn is_local(&self) -> bool {
        matches!(self.runtime, Some(Runtime::Local(_)))
    }

    pub(crate) fn uses_custom_store(&self) -> bool {
        match &self.runtime {
            Some(Runtime::Local(local)) => local.uses_custom_store(),
            _ => false,
        }
    }

    /// Fill in the client's defaults, then check what the bridge would reject.
    pub(crate) fn finish(mut self, default_api_key: Option<&str>) -> Result<proto::AgentOptions> {
        if self.api_key.is_none() {
            self.api_key = default_api_key.map(str::to_string);
        }
        // docs/protocol.md is explicit: always set the key on the options.
        // Not every bridge build falls back to CURSOR_API_KEY for every
        // operation, and the failure surfaces much later as "Invalid User API
        // Key" on the first Send.
        if self.api_key.as_deref().unwrap_or_default().is_empty() {
            return Err(Error::Config(
                "no Cursor API key: set it with Client::builder().api_key(..), \
                 AgentOptions::api_key(..), or the CURSOR_API_KEY environment variable"
                    .to_string(),
            ));
        }
        if self.is_local() && self.model.is_none() {
            return Err(Error::Config(
                "local agents require an explicit model; list the available ids with \
                 Client::models()"
                    .to_string(),
            ));
        }
        if self.runtime.is_none() {
            return Err(Error::Config(
                "an agent needs a runtime: AgentOptions::local(cwd) or AgentOptions::cloud(repo)"
                    .to_string(),
            ));
        }

        let (local, cloud) = match &self.runtime {
            Some(Runtime::Local(local)) => (Some(local.to_proto()?), None),
            Some(Runtime::Cloud(cloud)) => (None, Some(cloud.to_proto())),
            None => (None, None),
        };

        let mut sub_agents = std::collections::HashMap::new();
        for (name, definition) in &self.sub_agents {
            sub_agents.insert(name.clone(), definition.to_proto());
        }

        Ok(proto::AgentOptions {
            model: self.model.as_ref().map(ModelChoice::to_proto),
            api_key: self.api_key.unwrap_or_default(),
            name: self.name.unwrap_or_default(),
            local,
            cloud,
            mcp_servers: self
                .mcp_servers
                .iter()
                .map(|(name, server)| (name.clone(), server.to_proto()))
                .collect(),
            agents: sub_agents,
            agent_id: self.agent_id.unwrap_or_default(),
            mode: self
                .mode
                .map(|mode| mode.to_proto() as i32)
                .unwrap_or_default(),
            // A `ToolList` wrapper keeps "no built-in tools" distinct from
            // "the default toolset".
            tools: self.tools.map(|names| proto::ToolList { names }),
            disallowed_tools: self.disallowed_tools,
        })
    }
}

/// Per-message options.
#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    model: Option<ModelChoice>,
    mcp_servers: BTreeMap<String, McpServer>,
    force: Option<bool>,
    enable_deltas: bool,
    enable_steps: bool,
    mode: Option<AgentMode>,
    cloud_env_vars: BTreeMap<String, String>,
}

impl SendOptions {
    /// Default options: no deltas, no steps, the agent's own model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the model for this turn.
    #[must_use]
    pub fn model(mut self, model: impl Into<ModelChoice>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Add an MCP server for this turn.
    #[must_use]
    pub fn mcp_server(mut self, name: impl Into<String>, server: McpServer) -> Self {
        self.mcp_servers.insert(name.into(), server);
        self
    }

    /// Force the send even when the agent looks busy. Local agents only.
    #[must_use]
    pub fn force(mut self, force: bool) -> Self {
        self.force = Some(force);
        self
    }

    /// Emit raw streaming deltas as [`RunEvent::Delta`](crate::RunEvent::Delta).
    ///
    /// Off by default: without it the stream carries whole messages, which is
    /// what most callers want.
    #[must_use]
    pub fn deltas(mut self, enabled: bool) -> Self {
        self.enable_deltas = enabled;
        self
    }

    /// Emit completed conversation steps as [`RunEvent::Step`](crate::RunEvent::Step).
    #[must_use]
    pub fn steps(mut self, enabled: bool) -> Self {
        self.enable_steps = enabled;
        self
    }

    /// Override the conversation mode for this turn.
    #[must_use]
    pub fn mode(mut self, mode: AgentMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// A run-scoped environment variable. Cloud agents only.
    #[must_use]
    pub fn cloud_env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.cloud_env_vars.insert(key.into(), value.into());
        self
    }

    pub(crate) fn to_proto(&self) -> proto::SendOptions {
        proto::SendOptions {
            model: self.model.as_ref().map(ModelChoice::to_proto),
            mcp_servers: self
                .mcp_servers
                .iter()
                .map(|(name, server)| (name.clone(), server.to_proto()))
                .collect(),
            local: self
                .force
                .map(|force| proto::LocalSendOptions { force: Some(force) }),
            enable_deltas: self.enable_deltas,
            enable_steps: self.enable_steps,
            mode: self
                .mode
                .map(|mode| mode.to_proto() as i32)
                .unwrap_or_default(),
            cloud: (!self.cloud_env_vars.is_empty()).then(|| proto::CloudSendOptions {
                env_vars: self.cloud_env_vars.clone().into_iter().collect(),
            }),
        }
    }
}

/// An image attached to a prompt.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Image {
    /// A remote URL. Accepted only by cloud-routed agents in `sdk.v1`.
    Url {
        /// Where the image lives.
        url: String,
        /// Pixel dimensions, when known.
        dimension: Option<(u32, u32)>,
    },
    /// Inline image bytes.
    Data {
        /// Base64-encoded image bytes.
        base64: String,
        /// MIME type, for example `"image/png"`.
        mime_type: String,
        /// Pixel dimensions, when known.
        dimension: Option<(u32, u32)>,
    },
}

impl Image {
    /// A remote image URL. Accepted only by cloud-routed agents in `sdk.v1`.
    pub fn url(url: impl Into<String>) -> Self {
        Image::Url {
            url: url.into(),
            dimension: None,
        }
    }

    /// Inline image bytes, base64-encoded for you.
    pub fn bytes(data: impl AsRef<[u8]>, mime_type: impl Into<String>) -> Self {
        use base64::Engine as _;
        Image::Data {
            base64: base64::engine::general_purpose::STANDARD.encode(data),
            mime_type: mime_type.into(),
            dimension: None,
        }
    }

    /// Read an image from disk, inferring the MIME type from its extension.
    ///
    /// Falls back to `application/octet-stream` for an unrecognized extension,
    /// which the backend may reject — pass [`Image::bytes`] with an explicit
    /// type when you know better.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mime_type = match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            Some("svg") => "image/svg+xml",
            _ => "application/octet-stream",
        };
        Ok(Image::bytes(std::fs::read(path)?, mime_type))
    }

    /// Record the image's pixel dimensions.
    #[must_use]
    pub fn with_dimension(mut self, width: u32, height: u32) -> Self {
        match &mut self {
            Image::Url { dimension, .. } | Image::Data { dimension, .. } => {
                *dimension = Some((width, height));
            }
        }
        self
    }

    fn to_proto(&self) -> proto::SdkImage {
        let (source, dimension) = match self {
            Image::Url { url, dimension } => (
                proto::sdk_image::Source::Url(proto::SdkImageUrl { url: url.clone() }),
                *dimension,
            ),
            Image::Data {
                base64,
                mime_type,
                dimension,
            } => (
                proto::sdk_image::Source::Data(proto::SdkImageData {
                    data: base64.clone(),
                    mime_type: mime_type.clone(),
                }),
                *dimension,
            ),
        };
        proto::SdkImage {
            source: Some(source),
            dimension: dimension.map(|(width, height)| proto::SdkImageDimension { width, height }),
        }
    }
}

/// A message to send to an agent.
///
/// `"some text".into()` builds a plain text prompt.
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    /// The prompt text.
    pub text: String,
    /// Images to attach.
    pub images: Vec<Image>,
}

impl Prompt {
    /// A text-only prompt.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// Attach an image.
    #[must_use]
    pub fn with_image(mut self, image: Image) -> Self {
        self.images.push(image);
        self
    }

    pub(crate) fn to_proto(&self) -> proto::UserMessage {
        proto::UserMessage {
            text: self.text.clone(),
            images: self.images.iter().map(Image::to_proto).collect(),
        }
    }
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Prompt::text(text)
    }
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Prompt::text(text)
    }
}

impl From<&String> for Prompt {
    fn from(text: &String) -> Self {
        Prompt::text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_agent_needs_a_model() {
        let error = AgentOptions::local("/repo")
            .api_key("key")
            .finish(None)
            .unwrap_err();
        assert!(error.to_string().contains("require an explicit model"));
    }

    #[test]
    fn a_missing_api_key_is_caught_before_the_wire() {
        let error = AgentOptions::local("/repo")
            .model("composer-2.5")
            .finish(None)
            .unwrap_err();
        assert!(matches!(error, Error::Config(_)));
        assert!(error.to_string().contains("CURSOR_API_KEY"));
    }

    #[test]
    fn the_client_key_is_the_default() {
        let options = AgentOptions::local("/repo")
            .model("composer-2.5")
            .finish(Some("client-key"))
            .unwrap();
        assert_eq!(options.api_key, "client-key");
        assert_eq!(options.local.unwrap().cwd, vec!["/repo".to_string()]);
    }

    #[test]
    fn an_explicit_key_beats_the_client_default() {
        let options = AgentOptions::local("/repo")
            .model("composer-2.5")
            .api_key("agent-key")
            .finish(Some("client-key"))
            .unwrap();
        assert_eq!(options.api_key, "agent-key");
    }

    #[test]
    fn an_agent_needs_a_runtime() {
        let error = AgentOptions::new()
            .model("composer-2.5")
            .api_key("key")
            .finish(None)
            .unwrap_err();
        assert!(error.to_string().contains("needs a runtime"));
    }

    #[test]
    fn an_empty_tool_list_is_distinct_from_unset() {
        let none: Vec<String> = Vec::new();
        let with_empty = AgentOptions::local("/repo")
            .model("m")
            .api_key("k")
            .tools(none)
            .finish(None)
            .unwrap();
        assert_eq!(with_empty.tools, Some(proto::ToolList { names: vec![] }));

        let unset = AgentOptions::local("/repo")
            .model("m")
            .api_key("k")
            .finish(None)
            .unwrap();
        assert_eq!(unset.tools, None);
    }

    #[test]
    fn cloud_agents_do_not_require_a_model() {
        let options = AgentOptions::cloud("https://github.com/acme/repo")
            .api_key("key")
            .finish(None)
            .unwrap();
        let cloud = options.cloud.unwrap();
        assert_eq!(cloud.repos.len(), 1);
        assert!(options.model.is_none());
    }

    #[test]
    fn custom_tool_schemas_must_be_objects() {
        let tool = CustomTool::new("t", "d", serde_json::json!("not an object"));
        assert!(tool.to_proto().is_err());
    }

    #[test]
    fn agent_level_tools_win_over_client_wide_ones() {
        let mut local = LocalAgent::new("/repo").custom_tool(CustomTool::new(
            "lookup",
            "agent-level",
            serde_json::json!({"type": "object"}),
        ));
        local.merge_tools([CustomTool::new(
            "lookup",
            "client-wide",
            serde_json::json!({"type": "object"}),
        )]);
        let encoded = local.to_proto().unwrap();
        assert_eq!(
            encoded.custom_tools["lookup"].description.as_deref(),
            Some("agent-level")
        );
    }

    #[test]
    fn images_encode_from_bytes_and_urls() {
        let inline = Image::bytes(b"hello", "image/png").with_dimension(4, 2);
        let encoded = inline.to_proto();
        let Some(proto::sdk_image::Source::Data(data)) = encoded.source else {
            panic!("expected inline data");
        };
        assert_eq!(
            data.data, "aGVsbG8=",
            "bytes are base64-encoded for the caller"
        );
        assert_eq!(data.mime_type, "image/png");
        assert_eq!(encoded.dimension.unwrap().width, 4);

        let remote = Image::url("https://example.com/a.png").to_proto();
        assert!(matches!(
            remote.source,
            Some(proto::sdk_image::Source::Url(_))
        ));
    }

    #[test]
    fn a_prompt_is_built_from_a_bare_string() {
        let prompt: Prompt = "explain this".into();
        assert_eq!(prompt.to_proto().text, "explain this");
        assert!(prompt.images.is_empty());
    }

    #[test]
    fn a_no_repo_cloud_agent_sends_no_repositories() {
        let options = AgentOptions::new()
            .cloud_options(CloudAgent::no_repository())
            .api_key("k")
            .finish(None)
            .unwrap();
        assert!(options.cloud.unwrap().repos.is_empty());
    }

    #[test]
    fn send_options_only_set_cloud_when_asked() {
        assert!(SendOptions::new().to_proto().cloud.is_none());
        assert!(SendOptions::new()
            .cloud_env_var("A", "1")
            .to_proto()
            .cloud
            .is_some());
    }
}
