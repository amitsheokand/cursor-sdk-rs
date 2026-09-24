//! Bounded read/edit/run tools backed by the `toolgate` library crate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cursor_sdk::{CustomTool, ToolCall};
use serde_json::{json, Map, Value};
use toolgate::read::{format_numbered_lines, read, DEFAULT_RADIUS, WHOLE_FILE_LIMIT_LINES};
use toolgate::run::{run, DEFAULT_TIMEOUT_SECS, OUTPUT_CAP_BYTES};

use crate::fence::in_fence;
use crate::protocol::SeatRequest;

pub const TOOL_READ_WINDOW: &str = "read_window";
pub const TOOL_EDIT_DIFF: &str = "edit_diff";
pub const TOOL_RUN_BOUNDED: &str = "run_bounded";
pub const TOOL_RUN_GATES: &str = "run_gates";

pub const TOOL_READ_WINDOW_DESC: &str =
    "Read a bounded window of a file (line/radius or start/end; char budget)";
pub const TOOL_EDIT_DIFF_DESC: &str =
    "Exact-string replace in a file; returns a capped diff instead of re-reading";
pub const TOOL_RUN_BOUNDED_DESC: &str =
    "Run a program with argv (no shell), timeout, and clipped stdout/stderr";
pub const TOOL_RUN_GATES_DESC: &str =
    "Run declared packet gate commands (bash -lc) until the first failure";

pub const TOOLGATE_TOOLS: [(&str, &str); 4] = [
    (TOOL_READ_WINDOW, TOOL_READ_WINDOW_DESC),
    (TOOL_EDIT_DIFF, TOOL_EDIT_DIFF_DESC),
    (TOOL_RUN_BOUNDED, TOOL_RUN_BOUNDED_DESC),
    (TOOL_RUN_GATES, TOOL_RUN_GATES_DESC),
];

const BUILTIN_REPLACE: &[&str] = &["read", "edit", "write", "shell"];

/// Shared context for one seat attempt's toolgate tools.
#[derive(Debug, Clone)]
pub struct ToolgateContext {
    pub cwd: PathBuf,
    pub fence: Vec<String>,
    pub gates: Vec<String>,
}

impl ToolgateContext {
    pub fn from_request(request: &SeatRequest) -> Self {
        Self {
            cwd: PathBuf::from(&request.cwd),
            fence: request.fence.clone(),
            gates: request.toolgate.gates.clone(),
        }
    }
}

/// Apply `replace` disallowed built-ins before agent options are built.
pub fn apply_replace_disallowed(request: &mut SeatRequest) {
    if !request.toolgate.mode.is_replace() {
        return;
    }
    for name in BUILTIN_REPLACE {
        if !request.disallowed_tools.iter().any(|t| t == name) {
            request.disallowed_tools.push((*name).to_string());
        }
    }
}

/// Undo [`apply_replace_disallowed`] when the SDK rejects a disallowed name.
pub fn revert_replace_disallowed(request: &mut SeatRequest) {
    request
        .disallowed_tools
        .retain(|name| !BUILTIN_REPLACE.iter().any(|b| b == name));
}

/// Register toolgate tools when `mode` is `add` or `replace`.
pub async fn register_toolgate_tools(
    client: &cursor_sdk::Client,
    ctx: Arc<ToolgateContext>,
    keep: Option<&HashSet<String>>,
) {
    let declared = |name: &str| keep.map_or(true, |set| set.contains(name));

    let read_ctx = Arc::clone(&ctx);
    if declared(TOOL_READ_WINDOW) {
        let _ = client
            .register_tool(
                CustomTool::new(
                    TOOL_READ_WINDOW,
                    TOOL_READ_WINDOW_DESC,
                    schema(
                        &["path"],
                        json!({
                            "path": {"type": "string"},
                            "line": {"type": "integer", "description": "1-based center line"},
                            "radius": {"type": "integer", "description": "Lines each side of line (default 100)"},
                            "start": {"type": "integer", "description": "1-based range start"},
                            "end": {"type": "integer", "description": "1-based range end (inclusive)"},
                        }),
                    ),
                ),
                move |call: ToolCall| {
                    let ctx = Arc::clone(&read_ctx);
                    async move { Ok(invoke_tool_blocking(&ctx, TOOL_READ_WINDOW, call.args).await) }
                },
            )
            .await;
    }

    let edit_ctx = Arc::clone(&ctx);
    if declared(TOOL_EDIT_DIFF) {
        let _ = client
            .register_tool(
                CustomTool::new(
                    TOOL_EDIT_DIFF,
                    TOOL_EDIT_DIFF_DESC,
                    schema(
                        &["path", "old", "new"],
                        json!({
                            "path": {"type": "string"},
                            "old": {"type": "string"},
                            "new": {"type": "string"},
                            "all": {"type": "boolean"},
                        }),
                    ),
                ),
                move |call: ToolCall| {
                    let ctx = Arc::clone(&edit_ctx);
                    async move { Ok(invoke_tool_blocking(&ctx, TOOL_EDIT_DIFF, call.args).await) }
                },
            )
            .await;
    }

    let run_ctx = Arc::clone(&ctx);
    if declared(TOOL_RUN_BOUNDED) {
        let _ = client
            .register_tool(
                CustomTool::new(
                    TOOL_RUN_BOUNDED,
                    TOOL_RUN_BOUNDED_DESC,
                    schema(
                        &["program"],
                        json!({
                            "program": {"type": "string"},
                            "args": {"type": "array", "items": {"type": "string"}},
                            "timeout": {"type": "integer", "description": "Seconds (default 120)"},
                        }),
                    ),
                ),
                move |call: ToolCall| {
                    let ctx = Arc::clone(&run_ctx);
                    async move { Ok(invoke_tool_blocking(&ctx, TOOL_RUN_BOUNDED, call.args).await) }
                },
            )
            .await;
    }

    if !ctx.gates.is_empty() && declared(TOOL_RUN_GATES) {
        let gates_ctx = Arc::clone(&ctx);
        let _ = client
            .register_tool(
                CustomTool::new(TOOL_RUN_GATES, TOOL_RUN_GATES_DESC, schema(&[], json!({}))),
                move |_call: ToolCall| {
                    let ctx = Arc::clone(&gates_ctx);
                    async move { Ok(invoke_tool_blocking(&ctx, TOOL_RUN_GATES, json!({})).await) }
                },
            )
            .await;
    }
}

/// Execute a toolgate tool on a blocking thread (handlers and tests).
pub async fn invoke_tool_blocking(ctx: &ToolgateContext, name: &str, args: Value) -> Value {
    let ctx = ctx.clone();
    let name = name.to_string();
    match tokio::task::spawn_blocking(move || invoke_tool_sync(&ctx, &name, args)).await {
        Ok(value) => value,
        Err(join) => json!({"error": join.to_string()}),
    }
}

/// Synchronous toolgate dispatch (runs on a blocking thread in production).
pub fn invoke_tool_sync(ctx: &ToolgateContext, name: &str, args: Value) -> Value {
    let map = match args {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let root = ctx.cwd.as_path();
    let work = match name {
        TOOL_READ_WINDOW => read_window(root, &map),
        TOOL_EDIT_DIFF => edit_diff(root, &ctx.fence, &map),
        TOOL_RUN_BOUNDED => run_bounded(root, &map),
        TOOL_RUN_GATES => run_gates(root, &ctx.gates),
        other => Err(format!("unknown toolgate tool: {other}")),
    };
    match work {
        Ok(value) => value,
        Err(message) => json!({"error": message}),
    }
}

fn schema(required: &[&str], properties: Value) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

fn read_window(root: &Path, args: &Map<String, Value>) -> Result<Value, String> {
    let path = require_str(args, "path")?;
    let line = optional_u64(args, "line");
    let radius = optional_u64(args, "radius").unwrap_or(DEFAULT_RADIUS);
    let range = match (optional_u64(args, "start"), optional_u64(args, "end")) {
        (Some(start), Some(end)) => Some((start, end)),
        (None, None) => None,
        _ => return Err("start and end must both be set for a range".to_string()),
    };
    let hit = read(root, &path, line, radius, range, WHOLE_FILE_LIMIT_LINES)
        .map_err(|e| e.to_string())?;
    let numbered = format_numbered_lines(hit.start, &hit.text);
    Ok(json!({
        "path": hit.path,
        "start": hit.start,
        "end": hit.end,
        "total": hit.total,
        "lines": numbered,
    }))
}

fn edit_diff(root: &Path, fence: &[String], args: &Map<String, Value>) -> Result<Value, String> {
    let path = require_str(args, "path")?;
    if !fence.is_empty() {
        let rel = path_relative_to_cwd(root, &path)?;
        if !in_fence(&rel, root, fence) {
            return Err(format!("path outside fence: {rel}"));
        }
    }
    let old = require_str(args, "old")?;
    let new = require_str(args, "new")?;
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let hit = toolgate::edit::edit(root, &path, &old, &new, all).map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(hit).unwrap_or(json!({})))
}

fn run_bounded(root: &Path, args: &Map<String, Value>) -> Result<Value, String> {
    let program = require_str(args, "program")?;
    let argv = optional_string_list(args, "args");
    let timeout = optional_u64(args, "timeout").unwrap_or(DEFAULT_TIMEOUT_SECS);
    let hit = run(root, &program, &argv, timeout, OUTPUT_CAP_BYTES).map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(hit).unwrap_or(json!({})))
}

fn run_gates(root: &Path, gates: &[String]) -> Result<Value, String> {
    let mut results = Vec::new();
    for cmd in gates {
        let argv = vec!["-lc".to_string(), cmd.to_string()];
        let hit = run(root, "bash", &argv, DEFAULT_TIMEOUT_SECS, OUTPUT_CAP_BYTES)
            .map_err(|e| e.to_string())?;
        let exit = if hit.timed_out { -1 } else { hit.code };
        let tail = clip_gate_tail(&hit);
        results.push(json!({
            "cmd": cmd,
            "exit": exit,
            "timed_out": hit.timed_out,
            "tail": tail,
        }));
        if hit.timed_out || hit.code != 0 {
            break;
        }
    }
    Ok(json!({"gates": results}))
}

fn clip_gate_tail(hit: &toolgate::run::RunHit) -> String {
    const MAX: usize = 4000;
    let mut combined = hit.stdout.clone();
    if !hit.stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&hit.stderr);
    }
    if combined.chars().count() <= MAX {
        return combined;
    }
    let tail_chars: Vec<char> = combined.chars().collect();
    tail_chars[tail_chars.len() - MAX..].iter().collect()
}

fn require_str(args: &Map<String, Value>, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing or empty `{key}`"))
}

fn optional_u64(args: &Map<String, Value>, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

fn optional_string_list(args: &Map<String, Value>, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn path_relative_to_cwd(cwd: &Path, raw: &str) -> Result<String, String> {
    let path = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    let canon = path
        .canonicalize()
        .or_else(|_| cwd.join(raw).canonicalize())
        .map_err(|e| e.to_string())?;
    let cwd_canon = cwd.canonicalize().map_err(|e| e.to_string())?;
    let rel = canon
        .strip_prefix(&cwd_canon)
        .map_err(|_| "path escapes cwd".to_string())?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ctx(dir: &Path) -> ToolgateContext {
        ToolgateContext {
            cwd: dir.to_path_buf(),
            fence: vec!["allowed.txt".into()],
            gates: vec!["true".into(), "false".into()],
        }
    }

    #[tokio::test]
    async fn run_gates_stops_at_first_failure_and_clips() {
        let dir = std::env::temp_dir().join(format!("tg-gates-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let ctx = ctx(&dir);
        let out = invoke_tool_blocking(&ctx, TOOL_RUN_GATES, json!({})).await;
        let gates = out["gates"].as_array().expect("gates array");
        assert_eq!(gates.len(), 2);
        assert_eq!(gates[0]["exit"], 0);
        assert_eq!(gates[1]["exit"], 1);
    }

    #[tokio::test]
    async fn edit_diff_rejects_outside_fence() {
        let dir = std::env::temp_dir().join(format!("tg-fence-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("allowed.txt"), "a").unwrap();
        fs::write(dir.join("other.txt"), "a").unwrap();
        let ctx = ctx(&dir);
        let out = invoke_tool_blocking(
            &ctx,
            TOOL_EDIT_DIFF,
            json!({"path": "other.txt", "old": "a", "new": "b"}),
        )
        .await;
        assert!(out.get("error").is_some());
        let ok = invoke_tool_blocking(
            &ctx,
            TOOL_EDIT_DIFF,
            json!({"path": "allowed.txt", "old": "a", "new": "b"}),
        )
        .await;
        assert!(ok.get("error").is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
