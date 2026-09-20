//! The public vocabulary of this SDK.
//!
//! These are plain Rust types converted from the `sdk.v1` messages, so callers
//! never handle a `prost` struct, an `i32` enumeration, or a
//! `google.protobuf.Struct`. Free-form payloads arrive as
//! [`serde_json::Value`]. The raw contract stays available under
//! [`crate::proto`] for anyone who wants it.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value as JsonValue;

use crate::json::struct_to_json;
use crate::proto;

fn timestamp_to_system_time(value: Option<prost_types::Timestamp>) -> Option<SystemTime> {
    let value = value?;
    let nanos = value.nanos.clamp(0, 999_999_999) as u64;
    if value.seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(value.seconds as u64, nanos as u32))
    } else {
        UNIX_EPOCH.checked_sub(Duration::new(value.seconds.unsigned_abs(), 0))
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// A model plus the parameter values selected for it.
///
/// `"composer-2.5".into()` is the common case; [`ModelChoice::with_param`] adds
/// a parameter such as a reasoning level.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelChoice {
    /// Model identifier, for example `"composer-2.5"`.
    pub id: String,
    /// Parameter values, keyed by parameter id.
    pub params: BTreeMap<String, String>,
}

impl ModelChoice {
    /// A model with no parameter overrides.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            params: BTreeMap::new(),
        }
    }

    /// Set one model parameter, for example `("reasoning", "high")`.
    #[must_use]
    pub fn with_param(mut self, id: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(id.into(), value.into());
        self
    }

    pub(crate) fn to_proto(&self) -> proto::ModelSelection {
        proto::ModelSelection {
            id: self.id.clone(),
            params: self
                .params
                .iter()
                .map(|(id, value)| proto::ModelParameterValue {
                    id: id.clone(),
                    value: value.clone(),
                })
                .collect(),
        }
    }

    pub(crate) fn from_proto(source: proto::ModelSelection) -> Option<Self> {
        if source.id.is_empty() && source.params.is_empty() {
            return None;
        }
        Some(Self {
            id: source.id,
            params: source
                .params
                .into_iter()
                .map(|param| (param.id, param.value))
                .collect(),
        })
    }
}

impl From<&str> for ModelChoice {
    fn from(id: &str) -> Self {
        ModelChoice::new(id)
    }
}

impl From<String> for ModelChoice {
    fn from(id: String) -> Self {
        ModelChoice::new(id)
    }
}

impl std::fmt::Display for ModelChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.id)?;
        for (id, value) in &self.params {
            write!(formatter, " {id}={value}")?;
        }
        Ok(())
    }
}

/// One selectable value of a model parameter.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ModelParameterOption {
    /// The value to pass in [`ModelChoice::with_param`].
    pub value: String,
    /// Human-readable label.
    pub display_name: String,
}

/// A parameter a model accepts, with its permitted values.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ModelParameter {
    /// Parameter id.
    pub id: String,
    /// Human-readable label.
    pub display_name: String,
    /// Permitted values.
    pub values: Vec<ModelParameterOption>,
}

/// A named, pre-set combination of model parameters.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ModelVariant {
    /// The parameter values this variant selects.
    pub params: BTreeMap<String, String>,
    /// Human-readable label.
    pub display_name: String,
    /// Longer description.
    pub description: String,
    /// Whether this is the model's default variant.
    pub is_default: bool,
}

/// A model available to the authenticated account.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Model {
    /// Model identifier, as passed to [`ModelChoice::new`].
    pub id: String,
    /// Human-readable label.
    pub display_name: String,
    /// Longer description.
    pub description: String,
    /// Parameters this model accepts.
    pub parameters: Vec<ModelParameter>,
    /// Pre-set parameter combinations.
    pub variants: Vec<ModelVariant>,
}

impl Model {
    /// A [`ModelChoice`] selecting this model with no parameter overrides.
    pub fn choice(&self) -> ModelChoice {
        ModelChoice::new(&self.id)
    }
}

impl From<proto::SdkModel> for Model {
    fn from(source: proto::SdkModel) -> Self {
        Self {
            id: source.id,
            display_name: source.display_name,
            description: source.description,
            parameters: source
                .parameters
                .into_iter()
                .map(|parameter| ModelParameter {
                    id: parameter.id,
                    display_name: parameter.display_name,
                    values: parameter
                        .values
                        .into_iter()
                        .map(|value| ModelParameterOption {
                            value: value.value,
                            display_name: value.display_name,
                        })
                        .collect(),
                })
                .collect(),
            variants: source
                .variants
                .into_iter()
                .map(|variant| ModelVariant {
                    params: variant
                        .params
                        .into_iter()
                        .map(|param| (param.id, param.value))
                        .collect(),
                    display_name: variant.display_name,
                    description: variant.description,
                    is_default: variant.is_default,
                })
                .collect(),
        }
    }
}

/// The account an API key belongs to.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct User {
    /// Name given to the API key in the dashboard.
    pub api_key_name: String,
    /// Numeric Cursor user id.
    pub user_id: u64,
    /// Account email.
    pub email: String,
    /// Given name.
    pub first_name: String,
    /// Family name.
    pub last_name: String,
    /// Account creation time, as reported by Cursor Cloud.
    pub created_at: String,
}

impl From<proto::SdkUser> for User {
    fn from(source: proto::SdkUser) -> Self {
        Self {
            api_key_name: source.api_key_name,
            user_id: source.user_id,
            email: source.user_email,
            first_name: source.user_first_name,
            last_name: source.user_last_name,
            created_at: source.created_at,
        }
    }
}

/// A repository usable with cloud agents.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Repository {
    /// Git remote URL.
    pub url: String,
}

impl From<proto::SdkRepository> for Repository {
    fn from(source: proto::SdkRepository) -> Self {
        Self { url: source.url }
    }
}

/// Lifecycle state of an agent, as reported by listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AgentStatus {
    /// A run is executing.
    Running,
    /// The last run completed.
    Finished,
    /// The last run failed.
    Error,
    /// Unset, or a value newer than this crate.
    Unknown,
}

impl From<i32> for AgentStatus {
    fn from(value: i32) -> Self {
        match proto::AgentInfoStatus::try_from(value) {
            Ok(proto::AgentInfoStatus::Running) => AgentStatus::Running,
            Ok(proto::AgentInfoStatus::Finished) => AgentStatus::Finished,
            Ok(proto::AgentInfoStatus::Error) => AgentStatus::Error,
            _ => AgentStatus::Unknown,
        }
    }
}

/// Where an agent runs, and the runtime-specific facts about it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentRuntime {
    /// Runs on the machine hosting the bridge.
    Local {
        /// The agent's working directory.
        cwd: String,
    },
    /// Runs in Cursor's cloud, a self-hosted pool, or a named machine.
    Cloud {
        /// The execution environment.
        env: Option<CloudEnvironment>,
        /// Repository URLs the agent works against.
        repos: Vec<String>,
        /// Caller-owned tags set at creation.
        metadata: BTreeMap<String, String>,
    },
    /// The bridge did not report a runtime.
    Unknown,
}

/// A cloud execution environment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CloudEnvironment {
    /// Which kind of environment.
    pub kind: CloudEnvironmentKind,
    /// Pool or machine name, when the kind needs one.
    pub name: String,
}

/// The kind of cloud execution environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CloudEnvironmentKind {
    /// A Cursor-managed cloud VM.
    #[default]
    Cloud,
    /// A team self-hosted worker pool.
    Pool,
    /// A specific self-hosted machine.
    Machine,
    /// A value newer than this crate.
    Unknown,
}

impl CloudEnvironmentKind {
    pub(crate) fn to_proto(self) -> proto::CloudEnvironmentType {
        match self {
            CloudEnvironmentKind::Cloud => proto::CloudEnvironmentType::Cloud,
            CloudEnvironmentKind::Pool => proto::CloudEnvironmentType::Pool,
            CloudEnvironmentKind::Machine => proto::CloudEnvironmentType::Machine,
            CloudEnvironmentKind::Unknown => proto::CloudEnvironmentType::Unspecified,
        }
    }
}

impl From<proto::CloudEnvironment> for CloudEnvironment {
    fn from(source: proto::CloudEnvironment) -> Self {
        let kind = match proto::CloudEnvironmentType::try_from(source.r#type) {
            Ok(proto::CloudEnvironmentType::Cloud) => CloudEnvironmentKind::Cloud,
            Ok(proto::CloudEnvironmentType::Pool) => CloudEnvironmentKind::Pool,
            Ok(proto::CloudEnvironmentType::Machine) => CloudEnvironmentKind::Machine,
            _ => CloudEnvironmentKind::Unknown,
        };
        Self {
            kind,
            name: source.name,
        }
    }
}

/// Metadata about an agent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentInfo {
    /// The agent's id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Short summary of the conversation, when the backend has one.
    pub summary: String,
    /// Lifecycle state.
    pub status: AgentStatus,
    /// Last modification time.
    pub last_modified: Option<SystemTime>,
    /// Creation time.
    pub created_at: Option<SystemTime>,
    /// Whether the agent is archived.
    pub archived: bool,
    /// Where the agent runs.
    pub runtime: AgentRuntime,
}

impl From<proto::SdkAgentInfo> for AgentInfo {
    fn from(source: proto::SdkAgentInfo) -> Self {
        let runtime = match source.runtime_info {
            Some(proto::sdk_agent_info::RuntimeInfo::Local(local)) => {
                AgentRuntime::Local { cwd: local.cwd }
            }
            Some(proto::sdk_agent_info::RuntimeInfo::Cloud(cloud)) => AgentRuntime::Cloud {
                env: cloud.env.map(CloudEnvironment::from),
                repos: cloud.repos,
                metadata: cloud.metadata.into_iter().collect(),
            },
            None => AgentRuntime::Unknown,
        };
        Self {
            id: source.agent_id,
            name: source.name,
            summary: source.summary,
            status: source.status.into(),
            last_modified: timestamp_to_system_time(source.last_modified),
            created_at: timestamp_to_system_time(source.created_at),
            archived: source.archived,
            runtime,
        }
    }
}

/// Lifecycle state of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunStatus {
    /// The run is being set up.
    Creating,
    /// The run is executing.
    Running,
    /// The run completed successfully.
    Finished,
    /// The run failed.
    Error,
    /// The run was cancelled.
    Cancelled,
    /// The run outlived its deadline.
    Expired,
    /// Unset, or a value newer than this crate.
    Unknown,
}

impl RunStatus {
    /// Whether the run will produce no further events.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Finished | RunStatus::Error | RunStatus::Cancelled | RunStatus::Expired
        )
    }

    /// Whether the run finished successfully.
    pub fn is_success(self) -> bool {
        self == RunStatus::Finished
    }
}

impl From<i32> for RunStatus {
    fn from(value: i32) -> Self {
        match proto::RunLifecycleStatus::try_from(value) {
            Ok(proto::RunLifecycleStatus::Creating) => RunStatus::Creating,
            Ok(proto::RunLifecycleStatus::Running) => RunStatus::Running,
            Ok(proto::RunLifecycleStatus::Finished) => RunStatus::Finished,
            Ok(proto::RunLifecycleStatus::Error) => RunStatus::Error,
            Ok(proto::RunLifecycleStatus::Cancelled) => RunStatus::Cancelled,
            Ok(proto::RunLifecycleStatus::Expired) => RunStatus::Expired,
            _ => RunStatus::Unknown,
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            RunStatus::Creating => "creating",
            RunStatus::Running => "running",
            RunStatus::Finished => "finished",
            RunStatus::Error => "error",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Expired => "expired",
            RunStatus::Unknown => "unknown",
        };
        formatter.write_str(text)
    }
}

/// Token counts for a turn or an agent.
///
/// `total_tokens` excludes `reasoning_tokens`, which is a visibility-only
/// subset of the output tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TokenUsage {
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Tokens served from the prompt cache.
    pub cache_read_tokens: i64,
    /// Tokens written into the prompt cache.
    pub cache_write_tokens: i64,
    /// Total billed tokens.
    pub total_tokens: i64,
    /// Reasoning tokens, when the model reports them.
    pub reasoning_tokens: Option<i64>,
}

impl From<proto::TokenUsage> for TokenUsage {
    fn from(source: proto::TokenUsage) -> Self {
        Self {
            input_tokens: source.input_tokens,
            output_tokens: source.output_tokens,
            cache_read_tokens: source.cache_read_tokens,
            cache_write_tokens: source.cache_write_tokens,
            total_tokens: source.total_tokens,
            reasoning_tokens: source.reasoning_tokens,
        }
    }
}

/// Dollar cost of billed usage, in float cents.
///
/// Server-derived and eventually consistent after a run ends.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[non_exhaustive]
pub struct UsageCost {
    /// Undiscounted model token cost. Zero for request-priced usage.
    pub raw_cost_cents: f64,
    /// Amount actually charged.
    pub charged_cents: f64,
}

impl From<proto::UsageCost> for UsageCost {
    fn from(source: proto::UsageCost) -> Self {
        Self {
            raw_cost_cents: source.raw_cost_cents,
            charged_cents: source.charged_cents,
        }
    }
}

/// Usage for one run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunUsage {
    /// The run this usage belongs to.
    pub run_id: String,
    /// Token counts.
    pub usage: TokenUsage,
    /// Cost, when the backend reports it.
    pub cost: Option<UsageCost>,
}

/// Usage for an agent: totals plus a per-run breakdown.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentUsage {
    /// Totals across every run.
    pub usage: TokenUsage,
    /// Total cost, when the backend reports it.
    pub cost: Option<UsageCost>,
    /// Per-run breakdown.
    pub runs: Vec<RunUsage>,
}

impl From<proto::AgentUsage> for AgentUsage {
    fn from(source: proto::AgentUsage) -> Self {
        Self {
            usage: source.usage.map(TokenUsage::from).unwrap_or_default(),
            cost: source.cost.map(UsageCost::from),
            runs: source
                .runs
                .into_iter()
                .map(|run| RunUsage {
                    run_id: run.run_id,
                    usage: run.usage.map(TokenUsage::from).unwrap_or_default(),
                    cost: run.cost.map(UsageCost::from),
                })
                .collect(),
        }
    }
}

/// A branch a run worked on, and the pull request it opened.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GitBranch {
    /// Repository URL.
    pub repo_url: String,
    /// Branch name.
    pub branch: String,
    /// Pull-request URL, when one was opened.
    pub pr_url: String,
}

fn git_branches(info: Option<proto::RunGitInfo>) -> Vec<GitBranch> {
    info.map(|info| {
        info.branches
            .into_iter()
            .map(|branch| GitBranch {
                repo_url: branch.repo_url,
                branch: branch.branch,
                pr_url: branch.pr_url,
            })
            .collect()
    })
    .unwrap_or_default()
}

/// The outcome of a run.
///
/// Produced both by a terminal stream event and by the non-streaming
/// `WaitLiveRun` / `GetRun` paths, so the two agree on shape.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunOutcome {
    /// The run's id.
    pub run_id: String,
    /// The agent that owns the run.
    pub agent_id: String,
    /// Terminal (or current) lifecycle state.
    pub status: RunStatus,
    /// Final assistant text, when there is one.
    pub text: String,
    /// The model that produced the run.
    pub model: Option<ModelChoice>,
    /// Wall-clock duration.
    pub duration: Duration,
    /// Branches and pull requests the run produced.
    pub git: Vec<GitBranch>,
    /// When the run was created.
    pub created_at: Option<SystemTime>,
    /// Token usage, when reported.
    pub usage: Option<TokenUsage>,
    /// Machine-readable failure code, when the bridge supplied one.
    ///
    /// Often empty even for a failed run; [`RunOutcome::failure_message`] is
    /// the reliable place to look.
    pub error_code: Option<String>,
    /// Human-readable failure reason, taken from the last `status` message on
    /// the stream. `RunStreamResult.error_code` is frequently empty, so this is
    /// what to show a user.
    pub failure_message: Option<String>,
}

impl RunOutcome {
    pub(crate) fn from_result(source: proto::RunResult) -> Self {
        Self {
            run_id: source.run_id,
            agent_id: source.agent_id,
            status: source.status.into(),
            text: source.result,
            model: source.model.and_then(ModelChoice::from_proto),
            duration: Duration::from_millis(source.duration_ms),
            git: git_branches(source.git),
            created_at: timestamp_to_system_time(source.created_at),
            usage: source.usage.map(TokenUsage::from),
            error_code: None,
            failure_message: None,
        }
    }

    pub(crate) fn from_snapshot(source: proto::RunSnapshot) -> Self {
        Self {
            run_id: source.run_id,
            agent_id: source.agent_id,
            status: source.status.into(),
            text: source.result,
            model: source.model.and_then(ModelChoice::from_proto),
            duration: Duration::from_millis(source.duration_ms),
            git: git_branches(source.git),
            created_at: timestamp_to_system_time(source.created_at),
            usage: source.usage.map(TokenUsage::from),
            error_code: None,
            failure_message: None,
        }
    }

    pub(crate) fn from_stream_result(source: proto::RunStreamResult) -> Self {
        let status = RunStatus::from(source.status);
        let mut outcome = source
            .result
            .map(RunOutcome::from_result)
            .unwrap_or_else(|| RunOutcome {
                run_id: String::new(),
                agent_id: String::new(),
                status,
                text: String::new(),
                model: None,
                duration: Duration::ZERO,
                git: Vec::new(),
                created_at: None,
                usage: None,
                error_code: None,
                failure_message: None,
            });
        // The envelope's status is authoritative: RunResult may be a partial
        // snapshot on a failure.
        outcome.status = status;
        if outcome.run_id.is_empty() {
            outcome.run_id = source.run_id;
        }
        if outcome.agent_id.is_empty() {
            outcome.agent_id = source.agent_id;
        }
        outcome.error_code = source.error_code.and_then(non_empty);
        outcome
    }

    /// The best available description of why a run did not succeed.
    pub fn failure_reason(&self) -> Option<String> {
        if self.status.is_success() {
            return None;
        }
        self.failure_message
            .clone()
            .or_else(|| self.error_code.clone())
            .or_else(|| Some(format!("the run ended with status {}", self.status)))
    }
}

/// A message recorded against an agent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentMessage {
    /// Message discriminator, for example `"assistant"`.
    pub kind: String,
    /// Stable message id.
    pub uuid: String,
    /// The owning agent.
    pub agent_id: String,
    /// The message payload, shaped as the public SDK documents it.
    pub payload: JsonValue,
}

impl From<proto::AgentMessage> for AgentMessage {
    fn from(source: proto::AgentMessage) -> Self {
        Self {
            kind: source.r#type,
            uuid: source.uuid,
            agent_id: source.agent_id,
            payload: struct_to_json(source.message.as_ref()),
        }
    }
}

/// A file produced by a cloud agent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Artifact {
    /// Path, as passed to `download_artifact`.
    pub path: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Last-modified time, as reported by the artifact store.
    pub updated_at: String,
}

impl From<proto::SdkArtifact> for Artifact {
    fn from(source: proto::SdkArtifact) -> Self {
        Self {
            path: source.path,
            size_bytes: source.size_bytes,
            updated_at: source.updated_at,
        }
    }
}

/// What the bridge reports about its build and the contract it implements.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BridgeVersion {
    /// Bridge binary version.
    pub bridge_version: String,
    /// The contract version, expected to be `"sdk.v1"`.
    pub protocol_version: String,
    /// Capability strings for feature negotiation.
    pub capabilities: Vec<String>,
}

impl BridgeVersion {
    /// Whether the bridge advertises a capability, for example `"agent.usage"`.
    ///
    /// Unknown strings are forward-compatible additions, so gate optional
    /// features on this rather than on a version comparison.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|entry| entry == capability)
    }

    /// Whether the bridge speaks the contract this crate was generated from.
    pub fn speaks_supported_protocol(&self) -> bool {
        self.protocol_version == proto::PROTOCOL_VERSION
    }
}

impl From<proto::GetVersionResponse> for BridgeVersion {
    fn from(source: proto::GetVersionResponse) -> Self {
        Self {
            bridge_version: source.bridge_version,
            protocol_version: source.protocol_version,
            capabilities: source.capabilities,
        }
    }
}

/// A page of results plus the cursor that fetches the next one.
#[derive(Debug, Clone)]
pub struct Page<T> {
    /// The items on this page.
    pub items: Vec<T>,
    /// Cursor for the next page, or `None` when this is the last one.
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    pub(crate) fn new(items: Vec<T>, next_cursor: String) -> Self {
        Self {
            items,
            next_cursor: non_empty(next_cursor),
        }
    }

    /// Whether another page exists.
    pub fn has_more(&self) -> bool {
        self.next_cursor.is_some()
    }
}

impl<T> IntoIterator for Page<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choice_round_trips() {
        let choice = ModelChoice::new("composer-2.5").with_param("reasoning", "high");
        let restored = ModelChoice::from_proto(choice.to_proto()).unwrap();
        assert_eq!(restored, choice);
        assert_eq!(choice.to_string(), "composer-2.5 reasoning=high");
    }

    #[test]
    fn an_empty_model_selection_is_none() {
        assert!(ModelChoice::from_proto(proto::ModelSelection::default()).is_none());
    }

    #[test]
    fn unknown_enum_values_do_not_panic() {
        assert_eq!(RunStatus::from(9999), RunStatus::Unknown);
        assert_eq!(AgentStatus::from(-1), AgentStatus::Unknown);
    }

    #[test]
    fn stream_result_status_wins_over_the_embedded_snapshot() {
        // A failed run can carry a RunResult whose own status is stale.
        let source = proto::RunStreamResult {
            agent_id: "agent_1".into(),
            run_id: "run_1".into(),
            status: proto::RunLifecycleStatus::Error as i32,
            error_code: Some("model_error".into()),
            result: Some(proto::RunResult {
                status: proto::RunLifecycleStatus::Running as i32,
                result: "partial".into(),
                ..Default::default()
            }),
        };
        let outcome = RunOutcome::from_stream_result(source);
        assert_eq!(outcome.status, RunStatus::Error);
        assert_eq!(outcome.run_id, "run_1");
        assert_eq!(outcome.agent_id, "agent_1");
        assert_eq!(outcome.error_code.as_deref(), Some("model_error"));
        assert!(!outcome.status.is_terminal() || outcome.status.is_terminal());
    }

    #[test]
    fn failure_reason_prefers_the_status_message() {
        let mut outcome = RunOutcome::from_result(proto::RunResult {
            status: proto::RunLifecycleStatus::Error as i32,
            ..Default::default()
        });
        outcome.error_code = Some("unknown".into());
        outcome.failure_message = Some("the model refused the request".into());
        assert_eq!(
            outcome.failure_reason().as_deref(),
            Some("the model refused the request")
        );
    }

    #[test]
    fn a_successful_run_has_no_failure_reason() {
        let outcome = RunOutcome::from_result(proto::RunResult {
            status: proto::RunLifecycleStatus::Finished as i32,
            ..Default::default()
        });
        assert_eq!(outcome.failure_reason(), None);
    }

    #[test]
    fn pages_report_whether_more_exist() {
        let page = Page::new(vec![1, 2], String::new());
        assert!(!page.has_more());
        assert!(Page::new(vec![1], "cursor".to_string()).has_more());
    }
}
