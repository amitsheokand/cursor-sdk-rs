//! Seat protocol v1: serde types frozen by `PROTOCOL.md`.
//!
//! - stdin line 1 is [`SeatRequest`] (unknown fields rejected).
//! - stdin later lines are [`ControlInput`] (unknown fields rejected,
//!   deduplicated by `id` forever — dedup lives in P1 inbox).
//! - stdout is [`SeatEvent`] JSONL, every event carrying `seq`.
//! - the final event is `SeatEventKind::Result` carrying [`SeatResult`].
//!
//! Golden JSON tests in `tests/protocol_golden.rs` pin the wire shape.

use std::collections::HashMap;

/// Protocol version. `SeatRequest.v` must equal this.
pub const PROTOCOL_V: u32 = 1;

/// Default clip budget when `limits.clip_chars` is omitted.
pub const DEFAULT_CLIP_CHARS: usize = 40_000;

// ---- stdin: SeatRequest ----------------------------------------------------

/// Line 1 on stdin: everything the seat needs for one packet attempt.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatRequest {
    /// Protocol version, currently 1.
    pub v: u32,
    /// Stable `packet_id:attempt`, echoed back on every event/result.
    pub request_id: String,
    /// Local working directory the agent runs against.
    pub cwd: String,
    pub model: ModelRef,
    pub prompt: PromptPart,
    /// MCP servers to expose (names must match the catalog file).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Tools the agent must not get. Always includes `task`.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Enabled tool names (custom tools + MCP names).
    #[serde(default)]
    pub tools_enabled: Vec<String>,
    /// Directories containing `SKILL.md` files for the `skill_use` tool.
    #[serde(default)]
    pub skill_roots: Vec<String>,
    #[serde(default)]
    pub jev: JevConfig,
    pub limits: Limits,
    /// Directory holding `session.jsonl` for the durable run (P5).
    #[serde(default)]
    pub session_dir: Option<String>,
}

impl SeatRequest {
    /// Minimal validation P0 does before acknowledging the request.
    pub fn validate(&self) -> Result<(), String> {
        if self.v != PROTOCOL_V {
            return Err(format!("unsupported protocol v: {}", self.v));
        }
        if self.request_id.is_empty() {
            return Err("request_id is empty".to_string());
        }
        if self.cwd.is_empty() {
            return Err("cwd is empty".to_string());
        }
        if self.model.id.is_empty() {
            return Err("model.id is empty".to_string());
        }
        if !self.disallowed_tools.iter().any(|t| t == "task") {
            return Err("disallowed_tools must include \"task\"".to_string());
        }
        Ok(())
    }
}

/// `model {id, params{effort}}`, validated against `client.models()`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    pub id: String,
    #[serde(default)]
    pub params: ModelParams,
}

/// Typed model parameters. `fast` is not expressible: the seat always
/// runs non-fast (fixed law), forcing `fast=false` whenever the catalog
/// exposes it. Everything else is parametric.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelParams {
    /// e.g. `"high"`. Validated against the catalog per process (P4).
    #[serde(default)]
    pub effort: Option<String>,
    /// Any further catalog params, passed through opaquely.
    #[serde(default, flatten)]
    pub extra: HashMap<String, String>,
}

/// `prompt {task, effort_tag, body, steer}` assembled under
/// `limits.context_chars` by the P2 context builder.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptPart {
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub effort_tag: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub steer: Option<String>,
}

/// One MCP server entry. Kept deliberately loose: the seat forwards these
/// to `AgentOptions`; unknown keys are rejected so typos fail fast.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// `jev {enabled, questions_dir, self_check_turns, prune_tools}`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JevConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub questions_dir: Option<String>,
    #[serde(default)]
    pub self_check_turns: u32,
    /// Jev-routed tool exposure (Choice over offered tools, declared
    /// subset; confidence-gated fallback to all). Default off.
    #[serde(default)]
    pub prune_tools: bool,
}

/// `limits {context_chars, clip_chars=40000, timeout_s, heartbeat_s}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub context_chars: usize,
    #[serde(default = "default_clip_chars")]
    pub clip_chars: usize,
    pub timeout_s: u64,
    pub heartbeat_s: u64,
}

fn default_clip_chars() -> usize {
    DEFAULT_CLIP_CHARS
}

// ---- stdin: control inbox (P1) ----------------------------------------------

/// Later stdin lines: the unreal-agent inbox control message.
///
/// Unknown fields are rejected. Inputs are deduplicated by `id`, forever.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlInput {
    pub id: String,
    pub kind: ControlKind,
    pub mode: ControlMode,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<SettingsParams>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Control,
}

/// `hard | when_idle | heartbeat | settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    Hard,
    WhenIdle,
    Heartbeat,
    Settings,
}

/// `settings` parameters: effort for the next turn.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsParams {
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

// ---- stdout: SeatEvent -------------------------------------------------------

/// One stdout JSONL line. Every event carries `seq`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SeatEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub kind: SeatEventKind,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SeatEventKind {
    SeatStarted {
        request_id: String,
    },
    ContextReport {
        changes: Vec<ContextChange>,
    },
    RunStarted {
        run_id: String,
        agent_id: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        label: String,
        status: String,
        call_id: String,
    },
    Usage {
        #[serde(flatten)]
        usage: SeatUsage,
    },
    Status {
        status: String,
        #[serde(default)]
        message: Option<String>,
    },
    Heartbeat {},
    Resumed {
        run_id: String,
    },
    Jev {
        check: String,
        verdict: String,
        p: f64,
    },
    Result(SeatResult),
}

/// `changes[{kind: omitted|truncated|compacted, source, reason}]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextChange {
    pub kind: ContextChangeKind,
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextChangeKind {
    Omitted,
    Truncated,
    Compacted,
}

/// Token/cost usage. Mirrors the Python `_USAGE_FIELDS`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

// ---- stdout: final result ----------------------------------------------------

/// The terminal `result` event.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatResult {
    pub outcome: Outcome,
    pub status: String,
    /// The `ErrorKind` name (`RateLimited`, `Unknown`, …) or `None` on `ok`.
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
    pub request_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub archive_path: Option<String>,
    #[serde(default)]
    pub wall_ms: u64,
    #[serde(default)]
    pub ttfe_ms: Option<u64>,
    #[serde(default)]
    pub usage: Option<SeatUsage>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub self_check: Option<SelfCheck>,
    #[serde(default)]
    pub context_changes: Vec<ContextChange>,
    #[serde(default)]
    pub resumed: bool,
}

/// `ok|failed|startup_error|busy|bounced|stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Failed,
    StartupError,
    Busy,
    Bounced,
    Stale,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfCheck {
    pub passed: bool,
    pub turns: u32,
    #[serde(default)]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_enforces_task_law() {
        let mut req = minimal_request();
        assert!(req.validate().is_ok());
        req.disallowed_tools = vec!["other".into()];
        assert!(req.validate().is_err());
    }

    pub(crate) fn minimal_request() -> SeatRequest {
        SeatRequest {
            v: 1,
            request_id: "pkt:1".into(),
            cwd: "/repo".into(),
            model: ModelRef {
                id: "composer-2.5".into(),
                params: ModelParams {
                    effort: Some("high".into()),
                    extra: HashMap::new(),
                },
            },
            prompt: PromptPart {
                task: "t".into(),
                effort_tag: "high".into(),
                body: "do it".into(),
                steer: None,
            },
            mcp_servers: vec![],
            disallowed_tools: vec!["task".into()],
            tools_enabled: vec![],
            skill_roots: vec![],
            jev: JevConfig::default(),
            limits: Limits {
                context_chars: 100_000,
                clip_chars: 40_000,
                timeout_s: 600,
                heartbeat_s: 30,
            },
            session_dir: None,
        }
    }
}
