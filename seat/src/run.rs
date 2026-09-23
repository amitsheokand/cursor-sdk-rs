//! The core seat run (P4): one packet attempt against the bridge.
//!
//! [`run_seat`] owns the whole attempt: `seat_started`, pre-start with
//! P3 retry, `context_report` (P2), agent creation, the `select!` drive
//! loop (stream events vs heartbeat ticks vs inbox controls, plus a
//! deadline), and the final `result`. Every event is delivered to the
//! caller's channel with a contiguous `seq`.
//!
//! Cancel-safety note (per the rust-skills `async-cancel-safety` rule):
//! the `select!` branches construct a fresh `run.next_event()` future
//! every iteration and all accumulation lives in `Run` or the loop
//! locals outside the future, so dropping a losing branch only swallows
//! at most one keepalive. `inbox.recv()` is cancel-safe by construction.
//!
//! Pre-start-only retry is enforced structurally: [`should_retry_in_seat`]
//! is consulted only before `run_started`. After that the loop resumes
//! the stream (with replay dedup) instead of re-sending.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cursor_sdk::{
    Agent, AgentOptions, Client, Error, LocalAgent, McpServer, Model, ModelChoice, Run, RunEvent,
    RunOutcome, SendOptions, SettingSource, StreamMessage, TokenUsage,
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::clip::{archive_text, bound_output, bound_output_with_path};
use crate::fence::{attribute, changed, drift, rebaseline_entries, snapshot, Digest};
use crate::context::build_context;
use crate::inbox::Inbox;
use crate::jev::{
    JevHandle, PruneCandidate, SeatTools, CHECK_SELF, CHECK_TRIAGE, SEAT_TOOLS,
    TOOL_JEV_SCREEN, TOOL_JEV_VERIFY, TOOL_SKILL_USE,
};
use crate::protocol::{
    ControlMode, McpServerConfig, Outcome, SeatEvent, SeatEventKind, SeatRequest, SeatResult,
    SeatUsage, SelfCheck,
};
use crate::jev::register_tools;
use crate::retry::{
    classify, classify_run_status, retry_delay, should_retry_in_seat, RetryPolicy, SeatFailure,
};
use crate::session::{Opened, OpState, SessionError, SessionStore};

/// Cap on consecutive stream resumes; past it the seat stops replaying
/// and falls back to `WaitLiveRun` so a flapping stream cannot starve
/// heartbeat/control/deadline branches.
const MAX_RESUMES: u32 = 3;

/// Run one seat attempt, emitting every event (including the final
/// `result`) to `events` with contiguous `seq` from 0, and returning the
/// result. `inbox` delivers operator controls; the caller keeps feeding
/// it (via [`InboxHandle`](crate::inbox::InboxHandle)) while this runs.
///
/// The request is validated here (not just in the binary) so library
/// callers get the same `task`-disallowed / `fast=false` laws; an
/// invalid request ends in a `bounced` result with no agent contact.
///
/// When `session` is present the attempt is at-most-once: a stored
/// terminal result is replayed with no bridge contact, and a live run is
/// attached via `ObserveRun` instead of re-sent. Recording failures are
/// ignored (the run continues; durability degrades) — crashing loudly on
/// a full disk would be worse than a weakened at-most-once guarantee.
pub async fn run_seat(
    client: &Client,
    mut request: SeatRequest,
    mut inbox: Inbox,
    events: mpsc::UnboundedSender<SeatEvent>,
    mut session: Option<SessionStore>,
) -> SeatResult {
    let wall_start = Instant::now();
    let mut seq = 0;
    let mut emit = |kind: SeatEventKind| {
        seq += 1;
        let _ = events.send(SeatEvent { seq: seq - 1, kind });
    };

    if let Err(reason) = request.validate() {
        return terminal(
            &request,
            SeatFailure {
                outcome: Outcome::Bounced,
                error_kind: Some("Validation".to_string()),
                retryable: false,
                retry_after_ms: None,
                request_id: None,
            },
            reason,
            0,
            wall_start,
            Vec::new(),
            inbox,
            &mut session,
            None,
            &mut emit,
        )
        .await;
    }

    emit(SeatEventKind::SeatStarted {
        request_id: request.request_id.clone(),
    });

    // P2 context assembly (skills deferred to P6: no catalog entries yet).
    let built = build_context(&request, &[]);
    emit(SeatEventKind::ContextReport {
        changes: built.changes.clone(),
    });

    // Seat tools (P6): registered before the agent exists so their
    // declarations reach its options. Unavailable tools stay declared
    // and answer `not configured`.
    let seat_tools = Arc::new(SeatTools {
        jev: match (
            request.jev.enabled,
            request.jev.questions_dir.as_deref(),
        ) {
            (true, Some(dir)) => JevHandle::load(Path::new(dir)).ok(),
            _ => None,
        },
        skill_roots: request.skill_roots.clone(),
    });

    // Classify-then-act tool routing (opt-in): one cheap Noul per
    // offered tool in a single request; declare only the kept set.
    // Anything missing or failing keeps everything (fail-open).
    // MCP servers are filtered always; custom tools only restrict an
    // explicit allowlist (built-in names are unreliable to enumerate).
    let mut pruned: Option<HashSet<String>> = None;
    if request.jev.prune_tools {
        if let Some(jev) = seat_tools.jev.as_ref() {
            let mut candidates: Vec<PruneCandidate> = request
                .mcp_servers
                .iter()
                .map(|server| PruneCandidate {
                    name: server.name.clone(),
                    description: mcp_description(server),
                })
                .collect();
            for (name, description) in SEAT_TOOLS {
                candidates.push(PruneCandidate {
                    name: name.to_string(),
                    description: description.to_string(),
                });
            }
            let summary = format!(
                "{}\n{}\n{}",
                request.prompt.task,
                request.prompt.effort_tag,
                request.prompt.body.chars().take(2000).collect::<String>(),
            );
            if let Some((kept, floor)) =
                crate::jev::select_tools(jev, &summary, &candidates).await
            {
                emit(SeatEventKind::Jev {
                    check: crate::jev::SELECT_TOOLS_CHECK.to_string(),
                    verdict: kept.join(","),
                    p: floor,
                });
                request.mcp_servers
                    .retain(|server| kept.iter().any(|name| name == &server.name));
                pruned = Some(kept.into_iter().collect());
            }
        }
    }
    register_tools(client, Arc::clone(&seat_tools), pruned.as_ref()).await;

    // At-most-once dispatch: replay or attach before any pre-start work.
    // The decision is cloned out first so the arms can move the store.
    enum Dispatch {
        Replay(Option<SeatResult>),
        Resume(String, String),
        Fresh,
    }
    let dispatch = match session.as_ref() {
        Some(store) => match store.decision() {
            Opened::Replay => Dispatch::Replay(store.stored_result().cloned()),
            Opened::Resume { run_id, agent_id } => Dispatch::Resume(run_id, agent_id),
            Opened::Fresh => Dispatch::Fresh,
        },
        None => Dispatch::Fresh,
    };
    match dispatch {
        Dispatch::Replay(Some(stored)) => {
            emit(SeatEventKind::Result(stored.clone()));
            inbox.close().await;
            return stored;
        }
        Dispatch::Replay(None) => {
            // Unreachable: open() rejects terminal states without a
            // result. Fail closed rather than re-sending.
            return terminal(
                &request,
                SeatFailure {
                    outcome: Outcome::StartupError,
                    error_kind: None,
                    retryable: false,
                    retry_after_ms: None,
                    request_id: None,
                },
                "session is terminal without a stored result".to_string(),
                0,
                wall_start,
                built.changes.clone(),
                inbox,
                &mut session,
                seat_tools.jev.as_ref(),
                &mut emit,
            )
            .await;
        }
        Dispatch::Resume(run_id, agent_id) => {
            return attach(
                client,
                &request,
                run_id,
                agent_id,
                inbox,
                built.changes.clone(),
                wall_start,
                &mut emit,
                session,
                seat_tools.jev.clone(),
            )
            .await;
        }
        Dispatch::Fresh => {}
    }

    let policy = RetryPolicy::default();
    let mut attempts: u32 = 0;
    let key = request.request_id.clone();

    // Phase A: catalog read (idempotent) + model resolution. The catalog
    // is kept so self-check follow-ups can rebuild the choice.
    let catalog = match retry_op(&key, policy, &mut attempts, || client.models()).await {
        Ok(models) => models,
        Err(error) => {
            return terminal(
                &request,
                classify(&error),
                error.to_string(),
                attempts,
                wall_start,
                built.changes.clone(),
                inbox,
                &mut session,
                seat_tools.jev.as_ref(),
                &mut emit,
            )
            .await;
        }
    };
    let choice = match resolve_model(
        &catalog,
        &request.model.id,
        request.model.params.effort.as_deref(),
        &request.model.params.extra,
    ) {
        Ok(choice) => choice,
        Err(reason) => {
            return terminal(
                &request,
                SeatFailure {
                    outcome: Outcome::Bounced,
                    error_kind: Some("Validation".to_string()),
                    retryable: false,
                    retry_after_ms: None,
                    request_id: None,
                },
                reason,
                attempts,
                wall_start,
                built.changes.clone(),
                inbox,
                &mut session,
                seat_tools.jev.as_ref(),
                &mut emit,
            )
            .await;
        }
    };

    // Phase B: agent options (local validation, no wire).
    let options = match build_options(&request, choice, pruned.as_ref()) {
        Ok(options) => options,
        Err(reason) => {
            return terminal(
                &request,
                SeatFailure {
                    outcome: Outcome::Bounced,
                    error_kind: Some("Validation".to_string()),
                    retryable: false,
                    retry_after_ms: None,
                    request_id: None,
                },
                reason,
                attempts,
                wall_start,
                built.changes.clone(),
                inbox,
                &mut session,
                seat_tools.jev.as_ref(),
                &mut emit,
            )
            .await;
        }
    };

    // Phase C: create (no agent exists on failure: nothing leaks).
    let agent = match retry_op(&key, policy, &mut attempts, || client.create_agent(options.clone()))
        .await
    {
        Ok(agent) => agent,
        Err(error) => {
            return terminal(
                &request,
                classify(&error),
                error.to_string(),
                attempts,
                wall_start,
                built.changes.clone(),
                inbox,
                &mut session,
                seat_tools.jev.as_ref(),
                &mut emit,
            )
            .await;
        }
    };

    let cwd = PathBuf::from(&request.cwd);
    let allowed_extra: Vec<PathBuf> = request
        .session_dir
        .as_deref()
        .map(PathBuf::from)
        .into_iter()
        .collect();
    let guard_tools = !request.fence.is_empty() || !request.protected_roots.is_empty();
    let protected_roots: Vec<PathBuf> = request
        .protected_roots
        .iter()
        .map(PathBuf::from)
        .collect();
    let protected_before = if protected_roots.is_empty() {
        None
    } else {
        snapshot_async(&protected_roots, &request.fence, &cwd).await
    };

    // Phase D: open the stream on the same agent across retries.
    let run = match retry_op(&key, policy, &mut attempts, || {
        agent.send_with(built.text.clone(), SendOptions::new())
    })
    .await
    {
        Ok(run) => run,
        Err(error) => {
            let result = terminal(
                &request,
                classify(&error),
                error.to_string(),
                attempts,
                wall_start,
                built.changes.clone(),
                inbox,
                &mut session,
                seat_tools.jev.as_ref(),
                &mut emit,
            )
            .await;
            let _ = agent.close().await;
            return result;
        }
    };

    let handles = Handles {
        client,
        agent: Some(&agent),
        agent_id: agent.id().to_string(),
    };
    record(&mut session, |store| store.set_state(OpState::Awaiting));
    let (mut result, mut flags) = drive(
        &request,
        &handles,
        run,
        &mut inbox,
        &mut session,
        None,
        built.changes.clone(),
        attempts,
        wall_start,
        &mut emit,
        DriveFence {
            cwd: cwd.clone(),
            allowed_extra: allowed_extra.clone(),
            guard_tools,
            protected_roots: protected_roots.clone(),
            fence_entries: request.fence.clone(),
            protected_baseline: protected_before.clone(),
        },
    )
    .await;

    (result, flags) = apply_post_drive_fence(
        &request,
        &cwd,
        &allowed_extra,
        &protected_roots,
        result,
        flags,
        built.changes.clone(),
        attempts,
        wall_start,
        client,
        Some(&agent),
        &handles.agent_id,
        &handles,
        &mut inbox,
        &mut session,
        guard_tools,
        &mut emit,
    )
    .await;

    // P6 self-check: same-agent follow-up turns while the receipt check
    // fails. Only on success (a failed turn belongs to the drain's steer
    // loop), never when idle, at most self_check_turns follow-ups.
    // Classify-then-act cost control: a trivial task skips the loop
    // entirely (one cheap Noul instead of a full verification turn).
    let mut turns = 0u32;
    let mut check: Option<SelfCheck> = None;
    if let Some(jev) = seat_tools.jev.as_ref() {
        if request.jev.enabled
            && request.jev.self_check_turns > 0
            && result.outcome == Outcome::Ok
            && !flags.when_idle
            && !is_trivial(jev, &request).await
        {
            loop {
                if flags.when_idle {
                    break;
                }
                let diff = crate::jev::git_diff(&request.cwd).await;
                match crate::jev::self_check(jev, &result.text, &diff).await {
                    Ok(answer) if answer.passed => {
                        check = Some(SelfCheck {
                            passed: true,
                            turns,
                            reason: None,
                        });
                        break;
                    }
                    Ok(answer) => {
                        if turns >= request.jev.self_check_turns {
                            check = Some(SelfCheck {
                                passed: false,
                                turns,
                                reason: Some(format!(
                                    "{CHECK_SELF} p={:.2} after {turns} turn(s)",
                                    answer.p
                                )),
                            });
                            break;
                        }
                        let instructions = jev
                            .questions
                            .questions
                            .get(CHECK_SELF)
                            .map(|question| question.instructions.clone())
                            .unwrap_or_default();
                        let followup = format!(
                            "The self-check `{CHECK_SELF}` did not pass (p={:.2}). \
                             {instructions} Review the turn output against this check and fix \
                             what is missing, or end the turn if nothing is missing.",
                            answer.p
                        );
                        record(&mut session, |store| {
                            store.set_state(OpState::Awaiting)
                        });
                        let options = followup_options(
                            &catalog,
                            &request,
                            flags.effort.as_deref(),
                        );
                        match agent.send_with(followup, options).await {
                            Ok(next) => {
                                turns += 1;
                                let (next_result, next_flags) = drive(
                                    &request,
                                    &handles,
                                    next,
                                    &mut inbox,
                                    &mut session,
                                    None,
                                    Vec::new(),
                                    attempts,
                                    wall_start,
                                    &mut emit,
                                    DriveFence {
                                        cwd: cwd.clone(),
                                        allowed_extra: allowed_extra.clone(),
                                        guard_tools,
                                        protected_roots: protected_roots.clone(),
                                        fence_entries: request.fence.clone(),
                                        protected_baseline: protected_before.clone(),
                                    },
                                )
                                .await;
                                result = next_result;
                                flags = next_flags;
                                if result.outcome != Outcome::Ok {
                                    check = Some(SelfCheck {
                                        passed: false,
                                        turns,
                                        reason: Some(
                                            "follow-up turn did not succeed".to_string(),
                                        ),
                                    });
                                    break;
                                }
                            }
                            Err(_) => {
                                check = Some(SelfCheck {
                                    passed: false,
                                    turns,
                                    reason: Some("follow-up send failed".to_string()),
                                });
                                break;
                            }
                        }
                    }
                    // Jev trouble: keep the good result, record no check.
                    Err(_) => break,
                }
            }
        }
    }
    result.self_check = check;
    maybe_triage(seat_tools.jev.as_ref(), &result, &mut emit).await;
    emit(SeatEventKind::Result(result.clone()));
    handles.close_agent().await;
    inbox.close().await;
    result
}

/// Classify-then-act gate: ask the trivial-task Noul over the request.
/// Missing question, non-Noul shape, or any error means "not trivial"
/// (fail-open toward checking). One cheap call that can save a full
/// follow-up turn.
async fn is_trivial(jev: &crate::jev::JevHandle, request: &SeatRequest) -> bool {
    const CHECK: &str = "trivial_task";
    let Some(question) = jev.questions.questions.get(CHECK) else {
        return false;
    };
    if question.qtype != crate::jev::QuestionType::Noul {
        return false;
    }
    let state = serde_json::json!({
        "task": request.prompt.task,
        "effort_tag": request.prompt.effort_tag,
        "body": request.prompt.body.chars().take(2000).collect::<String>(),
    });
    match jev.ask(CHECK, &state).await.and_then(|a| crate::jev::parse_noul(&a)) {
        Ok(p) => p >= 0.5,
        Err(_) => false,
    }
}

/// Send options for a self-check follow-up: the agent's model, rebuilt
/// with a `settings` effort override when one arrived. An invalid
/// override falls back to the agent model rather than killing the turn.
fn followup_options(
    catalog: &[Model],
    request: &SeatRequest,
    effort: Option<&str>,
) -> SendOptions {
    match effort {
        Some(effort)
            if Some(effort) != request.model.params.effort.as_deref() =>
        {
            match resolve_model(
                catalog,
                &request.model.id,
                Some(effort),
                &request.model.params.extra,
            ) {
                Ok(choice) => SendOptions::new().model(choice),
                Err(_) => SendOptions::new(),
            }
        }
        _ => SendOptions::new(),
    }
}

/// Residue triage for the Unknown kind: one `triage` Choice over the
/// failed result, announced as a `jev` event. Skipped (fail-open) without
/// Jev, without the question, on non-Unknown kinds, and on
/// self-inflicted cancels.
async fn maybe_triage(
    jev: Option<&JevHandle>,
    result: &SeatResult,
    emit: &mut dyn FnMut(SeatEventKind),
) {
    let Some(jev) = jev else {
        return;
    };
    if result.error_kind.as_deref() != Some("Unknown") || result.status == "cancelled" {
        return;
    }
    let Ok((verdict, p)) = crate::jev::triage(jev, &result.status, &result.text).await else {
        return;
    };
    emit(SeatEventKind::Jev {
        check: CHECK_TRIAGE.to_string(),
        verdict,
        p,
    });
}

/// Retry `op` while it fails transiently, counting every try in
/// `attempts`. Returns the last error once attempts run out or the error
/// is terminal.
async fn retry_op<T, F, Fut>(
    key: &str,
    policy: RetryPolicy,
    attempts: &mut u32,
    mut op: F,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    loop {
        *attempts += 1;
        match op().await {
            Ok(value) => return Ok(value),
            Err(error)
                if should_retry_in_seat(&error) && *attempts < policy.max_attempts =>
            {
                let delay = retry_delay(policy, key, *attempts - 1, error.retry_after());
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Effort values that mean "no effort param", mirroring
/// `router/harness.py::_SKIP_EFFORT` (exact, case-sensitive).
const SKIP_EFFORT: [&str; 4] = ["", "n/a", "na", "none"];

/// Validate the requested model against the catalog and build the typed
/// choice: a skipped effort sends nothing; any other effort must name a
/// catalog value. `fast` is not a parameter: non-fast is fixed law, so
/// `fast=false` is forced whenever the catalog exposes it.
fn resolve_model(
    models: &[Model],
    model_id: &str,
    effort: Option<&str>,
    extra: &HashMap<String, String>,
) -> Result<ModelChoice, String> {
    let model = models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| format!("unknown model `{model_id}`"))?;
    let mut choice = ModelChoice::new(&model.id);
    if let Some(effort) = effort {
        if !SKIP_EFFORT.contains(&effort) {
            if let Some(param) = model
                .parameters
                .iter()
                .find(|param| param.values.iter().any(|value| value.value == effort))
            {
                choice = choice.with_param(&param.id, effort);
            } else if let Some(variant) = model
                .variants
                .iter()
                .find(|variant| variant.params.values().any(|value| value == effort))
            {
                for (id, value) in &variant.params {
                    choice = choice.with_param(id, value);
                }
            } else {
                return Err(format!(
                    "effort `{effort}` is not in the catalog for model `{}`",
                    model.id
                ));
            }
        }
    }
    if model.parameters.iter().any(|param| param.id == "fast") {
        choice = choice.with_param("fast", "false");
    }
    for (id, value) in extra {
        if !model.parameters.is_empty()
            && !model.parameters.iter().any(|param| &param.id == id)
        {
            return Err(format!(
                "unknown model param `{id}` for model `{}`",
                model.id
            ));
        }
        choice = choice.with_param(id, value);
    }
    Ok(choice)
}

/// One-line inventory description for an MCP server entry.
fn mcp_description(server: &McpServerConfig) -> String {
    if let Some(url) = server.url.as_deref() {
        return format!("http: {url}");
    }
    if let Some(command) = server.command.as_deref() {
        if server.args.is_empty() {
            return format!("stdio: {command}");
        }
        return format!("stdio: {command} {}", server.args.join(" "));
    }
    "unconfigured".to_string()
}

/// Local agent options: disallowed `task`, `setting_sources` off, MCP
/// servers from the request (`url` → HTTP, else `command` + `args`).
/// The seat's own tools are always registered (their declarations follow
/// the registry). An explicit `tools_enabled` allowlist is honored with
/// the seat tools unioned in; an empty list means unrestricted
/// (CLI-like), and an unavailable tool answers `not configured`.
fn build_options(
    request: &SeatRequest,
    choice: ModelChoice,
    pruned: Option<&HashSet<String>>,
) -> Result<AgentOptions, String> {
    let mut options = AgentOptions::local(&request.cwd).model(choice);
    if !request.tools_enabled.is_empty() {
        let mut enabled = request.tools_enabled.clone();
        for tool in [TOOL_SKILL_USE, TOOL_JEV_VERIFY, TOOL_JEV_SCREEN] {
            // Pruned seat tools stay out of an explicit allowlist.
            if pruned.map_or(true, |keep| keep.contains(tool))
                && !enabled.iter().any(|name| name == tool)
            {
                enabled.push(tool.to_string());
            }
        }
        options = options.tools(enabled);
    }
    options = options
        .disallowed_tools(request.disallowed_tools.clone())
        .local_options(
            LocalAgent::new(&request.cwd).setting_sources(Vec::<SettingSource>::new()),
        );
    for server in &request.mcp_servers {
        let mapped = if let Some(url) = server.url.as_deref() {
            McpServer::http(url)
        } else if let Some(command) = server.command.as_deref() {
            McpServer::stdio(command, server.args.clone())
        } else {
            return Err(format!(
                "mcp server `{}` needs `url` or `command`",
                server.name
            ));
        };
        options = options.mcp_server(&server.name, mapped);
    }
    Ok(options)
}

/// Who owns the run: a created agent (fresh path) or bare ids for an
/// attached run (resume path, where no agent handle exists).
struct Handles<'a> {
    client: &'a Client,
    agent: Option<&'a Agent>,
    agent_id: String,
}

impl Handles<'_> {
    async fn cancel_run(&self, run_id: &str) {
        match self.agent {
            Some(agent) => {
                let _ = agent.cancel_run(run_id).await;
            }
            None => {
                let _ = self.client.cancel_run(run_id, Some(&self.agent_id)).await;
            }
        }
    }

    async fn close_agent(&self) {
        if let Some(agent) = self.agent {
            let _ = agent.close().await;
        }
    }

    /// Best-effort post-run usage: the entry for this run, else agent
    /// totals. Unavailable on the resume path (no agent handle).
    async fn usage_for(&self, run_id: &str) -> Option<SeatUsage> {
        let usage = self.agent?.run_usage(run_id).await.ok()?;
        if let Some(entry) = usage.runs.iter().find(|entry| entry.run_id == run_id) {
            return Some(seat_usage_from_token(&entry.usage));
        }
        Some(seat_usage_from_token(&usage.usage))
    }
}

/// Best-effort session recording: failures are ignored (see [`run_seat`]).
fn record(
    session: &mut Option<SessionStore>,
    op: impl FnOnce(&mut SessionStore) -> Result<(), SessionError>,
) {
    if let Some(store) = session {
        let _ = op(store);
    }
}

fn op_for_outcome(outcome: Outcome, status: &str) -> OpState {
    match outcome {
        Outcome::Ok => OpState::Completed,
        Outcome::Failed if status == "cancelled" => OpState::Canceled,
        _ => OpState::Failed,
    }
}

/// Attach to a live run from a previous invocation instead of sending
/// again (at-most-once). The durable `ObserveRun` stream replays from
/// its start, so the transcript is re-emitted for the new consumer.
///
/// Leak note: the resumed run has no agent handle (the SDK exposes no
/// close-by-id), so the remote agent is never closed on this path — same
/// as the crashed process's agent. P7 needs a drain-side reaper or must
/// accept the accumulation.
#[allow(clippy::too_many_arguments)]
async fn attach(
    client: &Client,
    request: &SeatRequest,
    run_id: String,
    agent_id: String,
    mut inbox: Inbox,
    context_changes: Vec<crate::protocol::ContextChange>,
    wall_start: Instant,
    emit: &mut dyn FnMut(SeatEventKind),
    session: Option<SessionStore>,
    attach_jev: Option<JevHandle>,
) -> SeatResult {
    let mut session = session;
    let mut attempts: u32 = 0;
    let key = request.request_id.clone();
    let result = match retry_op(&key, RetryPolicy::default(), &mut attempts, || {
        client.observe_run(&run_id, None)
    })
    .await
    {
        Ok(run) => {
            let handles = Handles {
                client,
                agent: None,
                agent_id: agent_id.clone(),
            };
            // No self-check on attach: there is no agent handle for a
            // follow-up send, so the attached outcome stands as-is.
            let cwd = PathBuf::from(&request.cwd);
            let allowed_extra: Vec<PathBuf> = request
                .session_dir
                .as_deref()
                .map(PathBuf::from)
                .into_iter()
                .collect();
            let protected_roots: Vec<PathBuf> = request
                .protected_roots
                .iter()
                .map(PathBuf::from)
                .collect();
            let guard_tools = !request.fence.is_empty() || !request.protected_roots.is_empty();
            // On resume, protected-root snapshots only cover changes after re-attach.
            let protected_before = if protected_roots.is_empty() {
                None
            } else {
                snapshot_async(&protected_roots, &request.fence, &cwd).await
            };
            let (result, flags) = drive(
                request,
                &handles,
                run,
                &mut inbox,
                &mut session,
                Some((run_id.clone(), agent_id.clone())),
                context_changes.clone(),
                attempts,
                wall_start,
                emit,
                DriveFence {
                    cwd: cwd.clone(),
                    allowed_extra: allowed_extra.clone(),
                    guard_tools,
                    protected_roots: protected_roots.clone(),
                    fence_entries: request.fence.clone(),
                    protected_baseline: protected_before.clone(),
                },
            )
            .await;
            let (result, _flags) = apply_post_drive_fence(
                request,
                &cwd,
                &allowed_extra,
                &protected_roots,
                result,
                flags,
                context_changes,
                attempts,
                wall_start,
                client,
                None,
                &agent_id,
                &handles,
                &mut inbox,
                &mut session,
                guard_tools,
                emit,
            )
            .await;
            result
        }
        Err(error) => {
            return terminal(
                request,
                classify(&error),
                error.to_string(),
                attempts,
                wall_start,
                context_changes,
                inbox,
                &mut session,
                attach_jev.as_ref(),
                emit,
            )
            .await;
        }
    };
    maybe_triage(attach_jev.as_ref(), &result, emit).await;
    emit(SeatEventKind::Result(result.clone()));
    result
}

/// Tool-path fence inputs for the drive loop.
struct DriveFence {
    cwd: PathBuf,
    allowed_extra: Vec<PathBuf>,
    guard_tools: bool,
    protected_roots: Vec<PathBuf>,
    fence_entries: Vec<String>,
    protected_baseline: Option<Digest>,
}

/// Flags the drive loop hands back for post-turn decisions.
struct DriveFlags {
    when_idle: bool,
    effort: Option<String>,
    protected_baseline: Option<Digest>,
    fence_external_emitted: HashSet<String>,
}

impl From<&DriveState> for DriveFlags {
    fn from(state: &DriveState) -> Self {
        DriveFlags {
            when_idle: state.when_idle,
            effort: state.effort.clone(),
            protected_baseline: state.protected_baseline.clone(),
            fence_external_emitted: state.fence_external_emitted.clone(),
        }
    }
}

/// The `select!` drive loop: stream events vs heartbeat ticks vs inbox
/// controls, plus a deadline. Returns the terminal result (built and
/// recorded, but NOT emitted — the caller emits exactly one `result`
/// after any self-check follow-ups) plus drive flags.
#[allow(clippy::too_many_arguments)]
async fn drive(
    request: &SeatRequest,
    handles: &Handles<'_>,
    mut run: Run,
    inbox: &mut Inbox,
    session: &mut Option<SessionStore>,
    attached: Option<(String, String)>,
    context_changes: Vec<crate::protocol::ContextChange>,
    attempts: u32,
    wall_start: Instant,
    emit: &mut dyn FnMut(SeatEventKind),
    fence: DriveFence,
) -> (SeatResult, DriveFlags) {
    let heartbeat = Duration::from_secs(request.limits.heartbeat_s);
    let deadline = wall_start + Duration::from_secs(request.limits.timeout_s);
    let mut interval = tokio::time::interval(heartbeat.max(Duration::from_secs(1)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_activity = Instant::now();
    let mut controls_open = true;

    let mut state = DriveState::default();
    state.start = Some(wall_start);
    // Baseline snapshot was just taken; debounce mid-run samples (incl. interval's first tick).
    state.last_protected_snapshot = Some(Instant::now());
    if fence.protected_baseline.is_some() {
        state.protected_baseline = fence.protected_baseline.clone();
    }
    if let Some((run_id, agent_id)) = attached {
        // Resumed attach: the run is live remotely (ObserveRun
        // succeeded), so announce it up front. Controls queued before the
        // attach fire through the normal drain-first path below.
        state.run_id = Some(run_id.clone());
        state.agent_id = Some(agent_id.clone());
        state.run_started_emitted = true;
        state.resumed = true;
        emit(SeatEventKind::RunStarted { run_id, agent_id });
    }
    let outcome: RunOutcome = loop {
        // Drain ready controls first: a queued stop must never lose to a
        // fast stream. (Without this, N instantly-ready stream events can
        // beat a control still racing through the inbox task.)
        while let Ok(input) = inbox.try_recv() {
            apply_control(handles, input, &mut state, session).await;
        }
        tokio::select! {
            event = run.next_event() => {
                last_activity = Instant::now();
                match event {
                    Some(Ok(RunEvent::Message(message))) => {
                        observe_ids(handles, message.run_id(), &mut state, session, emit).await;
                        let is_tool_call = message.kind.as_str() == "tool_call";
                        let skip_fence_check =
                            is_tool_call && tool_call_fence_already_checked(&message, &mut state);
                        if fence.guard_tools && !skip_fence_check {
                            if let Some(path) = tool_fence_hit(
                                &message,
                                &fence.cwd,
                                &fence.allowed_extra,
                                &fence.protected_roots,
                            ) {
                                let tool = tool_label(&message);
                                emit_fence_escape(
                                    &path,
                                    &tool,
                                    &mut state,
                                    handles,
                                    session,
                                    emit,
                                )
                                .await;
                                continue;
                            }
                        }
                        emit_message(message, &mut state, emit);
                        if fence.guard_tools && is_tool_call {
                            maybe_protected_snapshot_escape(
                                &fence,
                                &mut state,
                                handles,
                                session,
                                emit,
                            )
                            .await;
                        }
                    }
                    Some(Ok(RunEvent::Completed(outcome))) => break *outcome,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        // Dropped stream: resume with replay dedup when a
                        // run id is known (capped, else WaitLiveRun), else
                        // fall back to WaitLiveRun directly.
                        if state.run_id.is_some() && state.resumptions < MAX_RESUMES {
                            match run.resume().await {
                                Ok(()) => {
                                    state.resumptions += 1;
                                    state.resumed = true;
                                    state.replaying = true;
                                    state.replay_pos = 0;
                                    emit(SeatEventKind::Resumed {
                                        run_id: state.run_id.clone().unwrap_or_default(),
                                    });
                                    continue;
                                }
                                Err(_) => {}
                            }
                        }
                        match run.wait().await {
                            Ok(outcome) => break outcome,
                            Err(error) => {
                                return (
                                    stream_failed(
                                        request, &error, &state, attempts, wall_start,
                                        context_changes, session,
                                    ),
                                    DriveFlags::from(&state),
                                );
                            }
                        }
                    }
                }
            }
            control = async {
                if controls_open { inbox.recv().await } else { std::future::pending().await }
            } => {
                match control {
                    Some(input) => apply_control(handles, input, &mut state, session).await,
                    None => controls_open = false,
                }
            }
            _ = interval.tick() => {
                if last_activity.elapsed() >= heartbeat && !heartbeat.is_zero() {
                    emit(SeatEventKind::Heartbeat {});
                    last_activity = Instant::now();
                }
                if fence.guard_tools {
                    maybe_protected_snapshot_escape(
                        &fence,
                        &mut state,
                        handles,
                        session,
                        emit,
                    )
                    .await;
                }
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                let _ = run.cancel().await;
                return (
                    timeout_result(request, &state, attempts, wall_start, context_changes, session),
                    DriveFlags::from(&state),
                );
            }
        }
    };

    let mut result =
        finish(request, outcome, &state, attempts, wall_start, context_changes, handles, session)
            .await;
    if let Some(path) = state.fence_escape.clone() {
        result = fence_escape_result(
            request,
            &path,
            &result,
            attempts,
            wall_start,
            result.context_changes.clone(),
            session,
        );
    }
    (result, DriveFlags::from(&state))
}

/// Mutable drive state; all of it lives outside the `select!` futures.
#[derive(Default)]
struct DriveState {
    run_id: Option<String>,
    agent_id: Option<String>,
    run_started_emitted: bool,
    pending_cancel: bool,
    #[allow(dead_code)]
    when_idle: bool,
    #[allow(dead_code)]
    effort: Option<String>,
    assistant_text: String,
    emitted_chunks: Vec<String>,
    replaying: bool,
    replay_pos: usize,
    seen_tool_calls: HashSet<String>,
    latest_usage: Option<SeatUsage>,
    ttfe_ms: Option<u64>,
    start: Option<Instant>,
    resumed: bool,
    resumptions: u32,
    fence_escape: Option<String>,
    last_protected_snapshot: Option<Instant>,
    fence_checked_tool_calls: HashSet<String>,
    protected_baseline: Option<Digest>,
    fence_external_emitted: HashSet<String>,
}

/// Record run/agent ids; on first sight emit `run_started` and fire a
/// queued hard cancel.
async fn observe_ids(
    handles: &Handles<'_>,
    run_id: Option<&str>,
    state: &mut DriveState,
    session: &mut Option<SessionStore>,
    emit: &mut dyn FnMut(SeatEventKind),
) {
    if state.run_id.is_none() {
        if let Some(run_id) = run_id {
            state.run_id = Some(run_id.to_string());
        }
    }
    if !state.run_started_emitted {
        if let Some(run_id) = state.run_id.clone() {
            let agent_id = handles.agent_id.clone();
            state.agent_id = Some(agent_id.clone());
            state.run_started_emitted = true;
            record(session, |store| {
                store.record_run(&run_id, &agent_id)
            });
            emit(SeatEventKind::RunStarted {
                run_id: run_id.clone(),
                agent_id,
            });
            if state.pending_cancel {
                state.pending_cancel = false;
                record(session, |store| store.set_state(OpState::Canceling));
                handles.cancel_run(&run_id).await;
            }
        }
    }
}

/// Apply one inbox control. Runs on delivery, which is the first-seen
/// gate: duplicates never reach here (the task drops them).
async fn apply_control(
    handles: &Handles<'_>,
    input: crate::protocol::ControlInput,
    state: &mut DriveState,
    session: &mut Option<SessionStore>,
) {
    record(session, |store| {
        store.record_control(&input.id)
    });
    match input.mode {
        ControlMode::Hard => {
            if let Some(run_id) = state.run_id.clone() {
                record(session, |store| store.set_state(OpState::Canceling));
                handles.cancel_run(&run_id).await;
            } else {
                state.pending_cancel = true;
            }
        }
        ControlMode::WhenIdle => {
            state.when_idle = true;
        }
        ControlMode::Settings => {
            state.effort = input
                .parameters
                .as_ref()
                .and_then(|params| params.reasoning_effort.clone());
        }
        ControlMode::Heartbeat => {}
    }
}

async fn snapshot_async(protected_roots: &[PathBuf], fence: &[String], cwd: &Path) -> Option<Digest> {
    let roots = protected_roots.to_vec();
    let fence = fence.to_vec();
    let cwd = cwd.to_path_buf();
    match tokio::task::spawn_blocking(move || snapshot(&roots, &fence, &cwd)).await {
        Ok(digest) => Some(digest),
        Err(_) => None,
    }
}

fn emit_fence_drift(paths: &[String], emit: &mut dyn FnMut(SeatEventKind)) {
    for path in paths {
        emit(SeatEventKind::Fence {
            kind: "drift".to_string(),
            path: path.clone(),
            tool: String::new(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_post_drive_fence(
    request: &SeatRequest,
    cwd: &Path,
    allowed_extra: &[PathBuf],
    protected_roots: &[PathBuf],
    mut result: SeatResult,
    mut flags: DriveFlags,
    context_changes: Vec<crate::protocol::ContextChange>,
    attempts: u32,
    wall_start: Instant,
    client: &Client,
    live_agent: Option<&Agent>,
    agent_id: &str,
    handles: &Handles<'_>,
    inbox: &mut Inbox,
    session: &mut Option<SessionStore>,
    guard_tools: bool,
    emit: &mut dyn FnMut(SeatEventKind),
) -> (SeatResult, DriveFlags) {
    let mut protected_baseline = flags.protected_baseline.take();
    let mut fence_external_emitted = std::mem::take(&mut flags.fence_external_emitted);
    if let Some(before) = protected_baseline.as_ref() {
        let Some(after) = snapshot_async(protected_roots, &request.fence, cwd).await else {
            flags.protected_baseline = protected_baseline;
            flags.fence_external_emitted = fence_external_emitted;
            return (result, flags);
        };
        let changed_paths = changed(before, &after);
        if !changed_paths.is_empty() {
            let roots = protected_roots.to_vec();
            let cwd_buf = cwd.to_path_buf();
            let after_copy = after.clone();
            let classified = tokio::task::spawn_blocking(move || {
                attribute(&changed_paths, &after_copy, &roots, &cwd_buf)
            })
            .await;
            if let Ok((escapes, external)) = classified {
                if let Some(base) = protected_baseline.as_mut() {
                    rebaseline_entries(base, &after, &external);
                }
                emit_fence_external(&external, &mut fence_external_emitted, emit);
                if !escapes.is_empty() {
                    result = fence_escape_result(
                        request,
                        &escapes
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        &result,
                        attempts,
                        wall_start,
                        context_changes,
                        session,
                    );
                }
            }
        }
    }

    if result.outcome == Outcome::Ok && !request.fence.is_empty() {
        let mut drift_paths = drift(cwd, &request.fence).await;
        if !drift_paths.is_empty() {
            emit_fence_drift(&drift_paths, emit);
            let listing = drift_paths.join("\n");
            let followup = format!(
                "These paths in the workspace are outside the packet fence and must be \
                 reverted or removed before the run can succeed:\n{listing}\n\
                 Revert or delete them, then end the turn."
            );
            record(session, |store| store.set_state(OpState::Awaiting));
            let owned_agent;
            let sender = if let Some(agent) = live_agent {
                agent
            } else {
                owned_agent = client.agent(agent_id.to_string());
                &owned_agent
            };
            match sender.send_with(followup, SendOptions::new()).await {
                Ok(next) => {
                    let (next_result, next_flags) = drive(
                        request,
                        handles,
                        next,
                        inbox,
                        session,
                        None,
                        Vec::new(),
                        attempts,
                        wall_start,
                        emit,
                        DriveFence {
                            cwd: cwd.to_path_buf(),
                            allowed_extra: allowed_extra.to_vec(),
                            guard_tools,
                            protected_roots: protected_roots.to_vec(),
                            fence_entries: request.fence.clone(),
                            protected_baseline: protected_baseline.clone(),
                        },
                    )
                    .await;
                    result = next_result;
                    flags.when_idle = next_flags.when_idle;
                    flags.effort = next_flags.effort;
                    protected_baseline = next_flags.protected_baseline;
                    fence_external_emitted = next_flags.fence_external_emitted;
                    if result.outcome == Outcome::Ok {
                        drift_paths = drift(cwd, &request.fence).await;
                    }
                    if result.outcome == Outcome::Ok && !drift_paths.is_empty() {
                        emit_fence_drift(&drift_paths, emit);
                        result = fence_drift_result(
                            request,
                            &drift_paths.join("\n"),
                            &result,
                            attempts,
                            wall_start,
                            session,
                        );
                    }
                }
                Err(_) => {
                    result = fence_drift_result(
                        request,
                        &listing,
                        &result,
                        attempts,
                        wall_start,
                        session,
                    );
                }
            }
        }
    }

    (
        result,
        DriveFlags {
            when_idle: flags.when_idle,
            effort: flags.effort,
            protected_baseline,
            fence_external_emitted,
        },
    )
}

fn tool_fence_hit(
    message: &StreamMessage,
    cwd: &Path,
    allowed_extra: &[PathBuf],
    protected_roots: &[PathBuf],
) -> Option<PathBuf> {
    if message.kind.as_str() != "tool_call" {
        return None;
    }
    let (name, args) = crate::fence::tool_args_from_message(message);
    crate::fence::tool_escape(&name, &args, cwd, allowed_extra, protected_roots)
}

/// Live fence runs once per `call_id` (started frame with args); completion replays skip re-check.
fn tool_call_fence_already_checked(message: &StreamMessage, state: &mut DriveState) -> bool {
    let call_id = str_field(message, "call_id")
        .or_else(|| str_field(message, "callId"))
        .unwrap_or_default();
    if call_id.is_empty() {
        return false;
    }
    !state
        .fence_checked_tool_calls
        .insert(call_id.to_string())
}

async fn emit_fence_escape(
    path: &Path,
    tool: &str,
    state: &mut DriveState,
    handles: &Handles<'_>,
    session: &mut Option<SessionStore>,
    emit: &mut dyn FnMut(SeatEventKind),
) {
    if state.fence_escape.is_some() {
        return;
    }
    emit(SeatEventKind::Fence {
        kind: "escape".to_string(),
        path: path.display().to_string(),
        tool: tool.to_string(),
    });
    state.fence_escape = Some(path.display().to_string());
    state.pending_cancel = true;
    if state.run_id.is_some() {
        record(session, |store| store.set_state(OpState::Canceling));
        handles
            .cancel_run(state.run_id.as_deref().unwrap_or_default())
            .await;
    }
}

fn emit_fence_external(
    paths: &[PathBuf],
    emitted: &mut HashSet<String>,
    emit: &mut dyn FnMut(SeatEventKind),
) {
    for path in paths {
        let key = path.display().to_string();
        if emitted.insert(key.clone()) {
            emit(SeatEventKind::Fence {
                kind: "external".to_string(),
                path: key,
                tool: String::new(),
            });
        }
    }
}

async fn maybe_protected_snapshot_escape(
    fence: &DriveFence,
    state: &mut DriveState,
    handles: &Handles<'_>,
    session: &mut Option<SessionStore>,
    emit: &mut dyn FnMut(SeatEventKind),
) {
    if state.fence_escape.is_some() || fence.protected_roots.is_empty() {
        return;
    }
    let Some(before) = state.protected_baseline.as_ref() else {
        return;
    };
    let now = Instant::now();
    let due = state
        .last_protected_snapshot
        .is_some_and(|t| now.duration_since(t) >= Duration::from_secs(1));
    if !due {
        return;
    }
    state.last_protected_snapshot = Some(now);
    let Some(after) =
        snapshot_async(&fence.protected_roots, &fence.fence_entries, &fence.cwd).await
    else {
        return;
    };
    let changed_paths = changed(before, &after);
    if changed_paths.is_empty() {
        return;
    }
    let roots = fence.protected_roots.clone();
    let cwd = fence.cwd.clone();
    let after_copy = after.clone();
    let classified = tokio::task::spawn_blocking(move || {
        attribute(&changed_paths, &after_copy, &roots, &cwd)
    })
    .await;
    let Ok((escapes, external)) = classified else {
        return;
    };
    if let Some(base) = state.protected_baseline.as_mut() {
        rebaseline_entries(base, &after, &external);
    }
    emit_fence_external(&external, &mut state.fence_external_emitted, emit);
    if let Some(path) = escapes.first() {
        emit_fence_escape(path, "", state, handles, session, emit).await;
    }
}

fn fence_escape_result(
    request: &SeatRequest,
    path_text: &str,
    prior: &SeatResult,
    attempts: u32,
    wall_start: Instant,
    context_changes: Vec<crate::protocol::ContextChange>,
    session: &mut Option<SessionStore>,
) -> SeatResult {
    let text = format!("fence escape: {path_text}");
    let (text, _) = bound_output(&text, request.limits.clip_chars);
    let result = SeatResult {
        outcome: Outcome::Bounced,
        status: "fence_escape".to_string(),
        error_kind: Some("FenceEscape".to_string()),
        retryable: false,
        retry_after_ms: None,
        request_id: request.request_id.clone(),
        run_id: prior.run_id.clone(),
        agent_id: prior.agent_id.clone(),
        model: Some(request.model.id.clone()),
        text,
        archive_path: None,
        wall_ms: wall_start.elapsed().as_millis() as u64,
        ttfe_ms: prior.ttfe_ms,
        usage: prior.usage.clone(),
        attempts,
        self_check: None,
        context_changes,
        resumed: prior.resumed,
    };
    record(session, |store| {
        store.record_result(&result)?;
        store.set_state(OpState::Failed)
    });
    result
}

fn fence_drift_result(
    request: &SeatRequest,
    path_listing: &str,
    prior: &SeatResult,
    attempts: u32,
    wall_start: Instant,
    session: &mut Option<SessionStore>,
) -> SeatResult {
    let text = format!("fence drift:\n{path_listing}");
    let (text, _) = bound_output(&text, request.limits.clip_chars);
    let result = SeatResult {
        outcome: Outcome::Failed,
        status: "fence_drift".to_string(),
        error_kind: Some("FenceDrift".to_string()),
        retryable: false,
        retry_after_ms: None,
        request_id: request.request_id.clone(),
        run_id: prior.run_id.clone(),
        agent_id: prior.agent_id.clone(),
        model: Some(request.model.id.clone()),
        text,
        archive_path: None,
        wall_ms: wall_start.elapsed().as_millis() as u64,
        ttfe_ms: prior.ttfe_ms,
        usage: prior.usage.clone(),
        attempts,
        self_check: None,
        context_changes: prior.context_changes.clone(),
        resumed: prior.resumed,
    };
    record(session, |store| {
        store.record_result(&result)?;
        store.set_state(OpState::Failed)
    });
    result
}

/// Map one stream message onto seat events, with replay dedup.
fn emit_message(
    message: StreamMessage,
    state: &mut DriveState,
    emit: &mut dyn FnMut(SeatEventKind),
) {
    match message.kind.as_str() {
        "assistant" => {
            if let Some(text) = message.text() {
                if text.is_empty() {
                    return;
                }
                // Replay dedup: the resumed stream replays from the start;
                // skip chunks already emitted in order, emit on divergence.
                if state.replaying {
                    if state.replay_pos < state.emitted_chunks.len()
                        && state.emitted_chunks[state.replay_pos] == text
                    {
                        state.replay_pos += 1;
                        return;
                    }
                    state.replaying = false;
                }
                if state.ttfe_ms.is_none() {
                    state.ttfe_ms = state.start.map(|start| start.elapsed().as_millis() as u64);
                }
                state.assistant_text.push_str(&text);
                state.emitted_chunks.push(text.clone());
                emit(SeatEventKind::Assistant { text });
            }
        }
        "tool_call" => {
            let label = tool_label(&message);
            let status = str_field(&message, "status").unwrap_or_default().to_string();
            let call_id = str_field(&message, "call_id")
                .or_else(|| str_field(&message, "callId"))
                .unwrap_or_default()
                .to_string();
            let dedupe_key = if call_id.is_empty() {
                None
            } else {
                Some(format!("{call_id}:{status}"))
            };
            if let Some(key) = dedupe_key {
                if !state.seen_tool_calls.insert(key) {
                    return;
                }
            }
            emit(SeatEventKind::ToolCall {
                label,
                status,
                call_id,
            });
        }
        "usage" => {
            if let Some(usage) = seat_usage_from(&message) {
                state.latest_usage = Some(usage.clone());
                emit(SeatEventKind::Usage { usage });
            }
        }
        "status" => {
            emit(SeatEventKind::Status {
                status: str_field(&message, "status").unwrap_or_default().to_string(),
                message: str_field(&message, "message").map(str::to_string),
            });
        }
        _ => {}
    }
}

fn str_field<'a>(message: &'a StreamMessage, key: &str) -> Option<&'a str> {
    message
        .payload
        .get(key)
        .or_else(|| message.payload.get("message")?.get(key))
        .and_then(Value::as_str)
}

/// SDK names every MCP call `mcp`: recover `server/tool` from the args,
/// ported from `runner/sdk_run.py::tool_label`.
pub fn tool_label(message: &StreamMessage) -> String {
    const DISPATCH: &[&str] = &[
        "mcp",
        "calldynamictool",
        "getdynamictools",
        "call_mcp_tool",
        "callmcp",
    ];
    let name = str_field(message, "name").unwrap_or_default().trim();
    let args = message
        .payload
        .get("args")
        .or_else(|| message.payload.get("message")?.get("args"));
    let mut mapping = serde_json::Map::new();
    if let Some(Value::Object(map)) = args {
        mapping = map.clone();
    } else if let Some(text) = args.and_then(Value::as_str) {
        if text.trim_start().starts_with('{') {
            if let Ok(Value::Object(map)) = serde_json::from_str(text) {
                mapping = map;
            }
        }
    }
    let field = |keys: &[&str]| {
        keys.iter()
            .filter_map(|key| mapping.get(*key)?.as_str())
            .map(str::trim)
            .find(|value| !value.is_empty())
            .unwrap_or_default()
            .to_string()
    };
    let namespace = field(&["namespace", "server", "mcp"]);
    let tool = field(&["toolName", "tool_name", "tool"]);
    if DISPATCH.contains(&name.to_lowercase().as_str()) || name.is_empty() {
        if !namespace.is_empty() && !tool.is_empty() {
            return format!("{namespace}/{tool}");
        }
        if !namespace.is_empty() {
            return namespace;
        }
        if !tool.is_empty() {
            return tool;
        }
        return if name.is_empty() {
            "mcp".to_string()
        } else {
            name.to_string()
        };
    }
    name.to_string()
}

/// Tolerant usage extraction: `usage` object or bare fields, any JSON
/// number type. Mirrors the five `_USAGE_FIELDS`.
fn seat_usage_from(message: &StreamMessage) -> Option<SeatUsage> {
    let root = message.payload.get("usage").unwrap_or(&message.payload);
    let number = |key: &str| {
        root.get(key)
            .and_then(|value| value.as_i64().or_else(|| value.as_u64().map(|v| v.min(i64::MAX as u64) as i64)))
    };
    let usage = SeatUsage {
        input_tokens: number("input_tokens").map(|v| v.max(0) as u64),
        output_tokens: number("output_tokens").map(|v| v.max(0) as u64),
        cache_read_tokens: number("cache_read_tokens").map(|v| v.max(0) as u64),
        cache_write_tokens: number("cache_write_tokens").map(|v| v.max(0) as u64),
        total_tokens: number("total_tokens").map(|v| v.max(0) as u64),
    };
    if usage == SeatUsage::default() {
        None
    } else {
        Some(usage)
    }
}

/// Assemble the terminal `result` for a completed run.
#[allow(clippy::too_many_arguments)]
async fn finish(
    request: &SeatRequest,
    outcome: RunOutcome,
    state: &DriveState,
    attempts: u32,
    wall_start: Instant,
    context_changes: Vec<crate::protocol::ContextChange>,
    handles: &Handles<'_>,
    session: &mut Option<SessionStore>,
) -> SeatResult {
    let status = outcome.status.to_string();
    let result_outcome = classify_run_status(outcome.status).unwrap_or(Outcome::Failed);
    let mut full_text = outcome.text.clone();
    if full_text.is_empty() {
        full_text = outcome.failure_reason().unwrap_or_default();
    }

    // Usage: stream result first, then what the stream already
    // delivered, then the post-run RPC. A GetUsage hiccup must not
    // discard usage the seat already saw.
    let usage = match outcome.usage.clone() {
        Some(usage) => Some(seat_usage_from_token(&usage)),
        None => state
            .latest_usage
            .clone()
            .or(best_effort_usage(handles, state.run_id.as_deref()).await),
    };

    let archive_path = match request.session_dir.as_deref() {
        Some(dir) if !full_text.is_empty() => archive_text(
            &full_text,
            std::path::Path::new(dir),
            "",
            &request.request_id,
            "result.txt",
        )
        .ok()
        .map(|archive| archive.path.display().to_string()),
        _ => None,
    };
    let text = match archive_path.as_deref() {
        Some(path) => bound_output_with_path(&full_text, request.limits.clip_chars, path).0,
        None => bound_output(&full_text, request.limits.clip_chars).0,
    };

    // A failed run with no RPC kind is unclassified by definition:
    // error_kind Unknown is P6's triage input (status tells triage
    // self-inflicted cancels apart from real failures).
    let result = SeatResult {
        outcome: result_outcome,
        status,
        error_kind: if result_outcome == Outcome::Failed {
            Some("Unknown".to_string())
        } else {
            None
        },
        retryable: false,
        retry_after_ms: None,
        request_id: request.request_id.clone(),
        run_id: state.run_id.clone().or_else(|| {
            Some(outcome.run_id.clone()).filter(|id| !id.is_empty())
        }),
        agent_id: state.agent_id.clone().or_else(|| {
            (!handles.agent_id.is_empty()).then(|| handles.agent_id.clone())
        }),
        model: Some(request.model.id.clone()),
        text,
        archive_path,
        wall_ms: wall_start.elapsed().as_millis() as u64,
        ttfe_ms: state.ttfe_ms,
        usage,
        attempts,
        self_check: None,
        context_changes,
        resumed: state.resumed,
    };
    // Result item before the terminal state: the crash invariant.
    record(session, |store| {
        store.record_result(&result)?;
        store.set_state(op_for_outcome(result.outcome, &result.status))
    });
    result
}

fn seat_usage_from_token(usage: &TokenUsage) -> SeatUsage {
    let positive = |value: i64| Some(value.max(0) as u64);
    SeatUsage {
        input_tokens: positive(usage.input_tokens),
        output_tokens: positive(usage.output_tokens),
        cache_read_tokens: positive(usage.cache_read_tokens),
        cache_write_tokens: positive(usage.cache_write_tokens),
        total_tokens: positive(usage.total_tokens),
    }
}

/// Best-effort post-run usage: prefer the entry for this run, else the
/// agent totals. Failures are ignored (usage stays whatever the stream
/// already reported).
async fn best_effort_usage(handles: &Handles<'_>, run_id: Option<&str>) -> Option<SeatUsage> {
    handles.usage_for(run_id?).await
}

/// The stream died unrecoverably after the run started: `failed` (never
/// `startup_error` — the run may still be executing remotely), keeping
/// the classifier's kind/retryable signals.
#[allow(clippy::too_many_arguments)]
fn stream_failed(
    request: &SeatRequest,
    error: &Error,
    state: &DriveState,
    attempts: u32,
    wall_start: Instant,
    context_changes: Vec<crate::protocol::ContextChange>,
    session: &mut Option<SessionStore>,
) -> SeatResult {
    let failure = classify(error);
    let mut text = state.assistant_text.clone();
    if text.is_empty() {
        text = error.to_string();
    }
    let (text, _) = bound_output(&text, request.limits.clip_chars);
    let result = SeatResult {
        outcome: Outcome::Failed,
        status: "stream_error".to_string(),
        error_kind: failure.error_kind,
        retryable: failure.retryable,
        retry_after_ms: None,
        request_id: failure
            .request_id
            .unwrap_or_else(|| request.request_id.clone()),
        run_id: state.run_id.clone(),
        agent_id: state.agent_id.clone(),
        model: Some(request.model.id.clone()),
        text,
        archive_path: None,
        wall_ms: wall_start.elapsed().as_millis() as u64,
        ttfe_ms: state.ttfe_ms,
        usage: state.latest_usage.clone(),
        attempts,
        self_check: None,
        context_changes,
        resumed: state.resumed,
    };
    record(session, |store| {
        store.record_result(&result)?;
        store.set_state(OpState::Failed)
    });
    result
}

/// The deadline fired: cancel best-effort, report `failed/timeout`.
#[allow(clippy::too_many_arguments)]
fn timeout_result(
    request: &SeatRequest,
    state: &DriveState,
    attempts: u32,
    wall_start: Instant,
    context_changes: Vec<crate::protocol::ContextChange>,
    session: &mut Option<SessionStore>,
) -> SeatResult {
    let (text, _) = bound_output(&state.assistant_text, request.limits.clip_chars);
    let result = SeatResult {
        outcome: Outcome::Failed,
        status: "timeout".to_string(),
        error_kind: Some("Unknown".to_string()),
        retryable: false,
        retry_after_ms: None,
        request_id: request.request_id.clone(),
        run_id: state.run_id.clone(),
        agent_id: state.agent_id.clone(),
        model: Some(request.model.id.clone()),
        text,
        archive_path: None,
        wall_ms: wall_start.elapsed().as_millis() as u64,
        ttfe_ms: state.ttfe_ms,
        usage: state.latest_usage.clone(),
        attempts,
        self_check: None,
        context_changes,
        resumed: state.resumed,
    };
    record(session, |store| {
        store.record_result(&result)?;
        store.set_state(OpState::Failed)
    });
    result
}

/// Terminal pre-start result: backfill `request_id`, gate
/// `retry_after_ms` to `busy`, clip the error text (nothing ran, so no
/// archive). Takes the inbox by value and closes it so early returns do
/// not leak the task.
async fn terminal(
    request: &SeatRequest,
    failure: SeatFailure,
    text: String,
    attempts: u32,
    wall_start: Instant,
    context_changes: Vec<crate::protocol::ContextChange>,
    inbox: Inbox,
    session: &mut Option<SessionStore>,
    jev: Option<&JevHandle>,
    emit: &mut dyn FnMut(SeatEventKind),
) -> SeatResult {
    inbox.close().await;
    let (text, _) = bound_output(&text, request.limits.clip_chars);
    let result = SeatResult {
        outcome: failure.outcome,
        status: outcome_name(failure.outcome).to_string(),
        error_kind: failure.error_kind,
        retryable: failure.retryable,
        retry_after_ms: if failure.outcome == Outcome::Busy {
            failure.retry_after_ms
        } else {
            None
        },
        request_id: failure
            .request_id
            .unwrap_or_else(|| request.request_id.clone()),
        run_id: None,
        agent_id: None,
        model: Some(request.model.id.clone()),
        text,
        archive_path: None,
        wall_ms: wall_start.elapsed().as_millis() as u64,
        ttfe_ms: None,
        usage: None,
        attempts,
        self_check: None,
        context_changes,
        resumed: false,
    };
    // Pre-start terminal: result item first, then close the operation.
    // (No run exists, so the op moves straight from Ready.)
    record(session, |store| {
        store.record_result(&result)?;
        store.set_state(OpState::Failed)
    });
    maybe_triage(jev, &result, emit).await;
    emit(SeatEventKind::Result(result.clone()));
    result
}

fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ok => "ok",
        Outcome::Failed => "failed",
        Outcome::StartupError => "startup_error",
        Outcome::Busy => "busy",
        Outcome::Bounced => "bounced",
        Outcome::Stale => "stale",
    }
}
