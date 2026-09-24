# Seat protocol v1 (frozen in P0)

This file is the source of truth. `src/protocol.rs` is the serde
implementation; `tests/protocol_golden.rs` pins the wire shape. Later
packets code against this document — do not change it without a version
bump and a board decision.

Per the TypeSafe skill: Jev question shapes live in TOML config under
`questions_dir`, not in code. The seat posts `state + questions` to
`POST https://api.typesafe.ai/v1/systemone` with `model: jev-1.13.0`
(pinned per plan), and maps answers onto `jev` events / tool results.
`jev_verify` follows the citation-check cookbook (Choice:
supports/contradicts/says_nothing → verified/contradicted/unsupported,
plus a string-match `fabricated` path); `jev_screen` follows the
llm-guardrails cookbook (Noul battery + Score severity, thresholds in
code/config).

## stdin

Line 1 is `SeatRequest` (`v`, `request_id` stable `packet_id:attempt`,
`cwd`, `model {id, params{effort}}` (`fast` is not expressible: fixed
non-fast law, forced when the catalog exposes it), `prompt {task, effort_tag, body,
steer}`, `mcp_servers`, `disallowed_tools` always including `task`,
`tools_enabled` (empty = unrestricted, CLI-like; non-empty is an
allowlist with the seat tools unioned in), `skill_roots`, `jev {enabled, questions_dir,
self_check_turns, prune_tools}`, `toolgate {mode: off|add|replace, gates?}` (default
`off`; `gates` is the trusted gate-command list for `run_gates`), `limits {context_chars, clip_chars=40000,
timeout_s, heartbeat_s}`, `session_dir`, `fence` (worktree-relative file or
directory prefix allowlist; default `[]`), `protected_roots` (absolute checkout
paths that must not change under the fence entries; default `[]`)). Unknown
fields are rejected. `cwd` must be absolute, exist, and is canonicalized at
validation.

Later lines are control input (unreal-agent inbox):
`{id, kind:"control", mode: hard|when_idle|heartbeat|settings, reason,
parameters}`. Unknown fields are rejected. Inputs are deduplicated by
`id`, forever. A `hard` stop arriving before `run_id` exists is queued,
then fires `agent.cancel_run` on `run_started`. `when_idle` finishes the
turn and blocks follow-ups. `settings` changes effort for the next turn.

Cross-rules (enforced at parse; invalid states never constructed):
`hard|when_idle|heartbeat` accept no `parameters` — the key must be
absent, and an explicit `null` counts as present (mirroring Go).
`heartbeat` requires a non-empty `reason`. `settings` requires
`parameters` to be an object with exactly
`{reasoning_effort: low|medium|high|xhigh|max}`. Serialized control
inputs omit `parameters` when unset. Intake is an unbounded channel so
`submit` never blocks; the producer is stdin-paced and the dedup set
caps distinct ids.

Failure envelope: an invalid stdin **line 1** is fail-fast — exit 1
with a stderr message and no `result` event. An invalid **control**
line mid-run does not rewrite history: the run continues on the valid
prefix, the `result` event stands, and the fault is reported via a
nonzero exit only. P7 maps a non-zero exit before any stdout event to
`startup_error{error_kind: Unknown}`; the `result` event envelope for
run failures arrives in P4. Controls arriving after the result are moot
(the inbox is closed) and ignored.

## stdout

`SeatEvent` JSONL; every event has `seq` (u64, contiguous from 0):

- `seat_started {request_id}`
- `context_report {changes[{kind: omitted|truncated|compacted, source,
  reason}]}`
- `run_started {run_id, agent_id}`
- `assistant {text}`
- `tool_call {label, status, call_id}` (`label` recovers MCP
  server/tool, ported from `runner/sdk_run.py`)
- `usage {input_tokens?, output_tokens?, cache_read_tokens?,
  cache_write_tokens?, total_tokens?}` (the five `_USAGE_FIELDS`)
- `status {status, message?}`
- `heartbeat {}` (every `heartbeat_s` even when the model is silent)
- `resumed {run_id}`
- `jev {check, verdict, p}` (`check` names the TOML question;
  `verdict` is the Choice/Noul outcome; `p` its probability)
- `fence {kind, path, tool}` (`kind` is `escape`, `external`, or `drift`; `path` is the
  offending path; `tool` is the tool name when applicable). `external` is a notice
  only (outcome unchanged).
- Live escape checks inspect tool path/`cwd` arguments. For shell tools, the
  seat also tokenizes the command string (`command` / `cmd` / `script`, including
  nested `arguments`) on whitespace and shell metacharacters, and treats
  absolute or home-relative tokens (and `KEY=value` / `--flag=value` path
  suffixes) as path candidates. Relative tokens are joined to the effective
  shell `cwd` (from shell args when set, else the worktree `cwd`), then
  lexically normalized without following symlinks. Within one command, `cd`
  / `pushd` updates that lexical base for later tokens (left to right). A
  candidate escapes when it lies under a `protected_root` and is not under
  `cwd` or `session_dir`.
- During the drive loop, protected-root snapshots are re-run after each
  `tool_call` (debounced to at most once per second) and on every heartbeat
  tick. Changes from the baseline are attributed in one blocking pass (snapshot,
  hash protected/worktree copies, then git). Escape iff the protected file is a
  regular file, its content hash matches the worktree copy, **and** the path is
  dirty in the protected repo (`git -C <toplevel> status --porcelain` with
  `:(literal)` pathspecs under `rev-parse --show-prefix`). Owner commit/pull/revert/stash
  (primary clean) is `external`. Git status failure mid-run leaves the path
  undecided (no event, no rebaseline, retry next tick); post-drive undecided
  paths emit `external` with `tool: "undecided"` without rebaseline or dedup
  (outcome unchanged; self-check may retry). On resume attach, the protected
  baseline is taken at re-attach time — only changes after attach are reported.
  Any
  other change (different content, deletion, symlink, missing worktree file) is
  external: one `fence` `external` event per path, baseline updated so the same
  edit is not re-reported, outcome unchanged. The post-drive snapshot uses the
  same attribution as a backstop (`escape` only changes the outcome).
- final `result`

`result`: `outcome: ok|failed|startup_error|busy|bounced|stale`,
`status`, `error_kind` (the `ErrorKind` name; a post-start `failed` run
with no RPC kind reports `Unknown`, which is P6's triage input; fence
violations use `FenceEscape` or `FenceDrift`),
`retryable`,
`retry_after_ms`, `request_id` (full, never truncated), `run_id`,
`agent_id`, `model`, `text` (clipped), `archive_path`, `wall_ms`,
`ttfe_ms`, `usage`, `attempts` (cumulative pre-start tries across the
models/create/send phases, not send-retries), `self_check {passed,
turns, reason?}`, `context_changes`, `resumed`, `tool_stats` (`{tool_name:
{calls, result_chars}}` for built-in and custom tools).

Tagged `type` uses snake_case (`seat_started`, `context_report`,
`run_started`, `assistant`, `tool_call`, `usage`, `status`,
`heartbeat`, `resumed`, `jev`, `result`). Stdout event variants
tolerate unknown fields (forward-compatible); stdin types reject them.
`model.params.extra` is passthrough: unknown `params.*` keys are kept,
not rejected — P4's catalog validation against `client.models()` is the
backstop for typos there.

Deliberate P0 superset: `result` also carries `context_changes` and
`resumed` (needed by P7 `append_run_record` extras); P7 codes against
them as frozen.

## Jev and tools (P6)

- Tools are the enabled name set plus the seat's own `skill_use`,
  `jev_verify`, `jev_screen`, always registered before the agent exists.
  An enabled tool whose dependency is missing (no Jev key, no skill
  roots) stays declared and answers `{"error": "tool X is not
  configured: ..."}`.
- `skill_use {skill}` loads `<root>/<skill>/SKILL.md` on call only
  (path traversal rejected, 64KB cap); the context carries the catalog
  note (names only).
- Native Jev: `POST {TYPESAFE_ENDPOINT or
  https://api.typesafe.ai/v1/systemone}` with `model: jev-1.13.0`
  (pinned), 4s timeout, fail-closed. Key from `TYPESAFE_API_KEY` or
  `~/.config/typesafe.env` (0600-enforced, keys.py format), never
  logged (redacting newtype).
- Question shapes come from `questions_dir/*.toml` (`[question.<id>]`
  with `type|instructions` plus `options`/`criteria_*` (choice),
  `criteria_true|false` (noul), `levels` (score); optional
  `[thresholds]` with `<id>_at` pass boundaries, e.g.
  `receipt_supported_at`), mirroring `gates/questions/*.toml`. The seat ships
  defaults in `seat/questions/` — P7 points `questions_dir` there.
  Default check ids: `jev_verify` → `relation`, triage → `triage`,
  self-check → `receipt_supported` (must be a Noul),
  tool prune → `select_tools` (Noul template with `{name}`/`{description}`).
- Tool pruning (opt-in `jev.prune_tools`): one Noul per offered tool
  (MCP servers + seat customs) in a single request; only `p < 0.4` is
  dropped, uncertainty stays declared. MCP servers filter from the
  request; customs skip registration (under explicit allowlists only —
  built-in names are unreliable to enumerate). Announced as a
  `jev{select_tools}` event.
- `jev_verify {claim, section, quote?, check?}`: absent quote with no
  substring match is `fabricated` (no model call); else one Choice →
  `verified|contradicted|unsupported` with confidence.
- `jev_screen {text, side?, checks?}`: default battery is every
  Noul plus the first Score (as severity) in id order, one request;
  uniform strict routing (block ≥0.70, review ≥0.35, severity ≥2.0
  upgrades review to block).
- Same-agent self-check (fresh path only, C arm): after an `ok` turn,
  ask `receipt_supported` over the clipped text plus `git diff`
  (capped); pass at p≥0.5. On fail send one follow-up naming the check
  and its p (up to `self_check_turns`, never when idle); a `settings`
  effort override rebuilds the model choice for the follow-up.
  Recorded as `result.self_check {passed, turns, reason?}`.
  Classify-then-act cost control: a `trivial_task` Noul over the
  request skips the loop entirely (one cheap call instead of a full
  verification turn); the skip needs p≥0.5 at confidence≥0.7
  (confidence-gated, cookbook pattern), else fail-open toward checking.
  Ships in `seat/questions/trivial.toml`.
- Residue triage: only `error_kind: Unknown` (and never
  `status: cancelled`) goes to the `triage` Choice; the verdict is
  announced as a `jev` event. Missing question or Jev trouble skips
  triage fail-open.

## Compatibility notes

- Archive header is byte-compatible with `jobs/clip.py` `wrap_receipt`
  (`sdkrun_v1` plus sha256) so the Python post gate reads it unchanged.
  Verified byte-for-byte against the Python output (including the double
  space before `OR`); `clip.rs::receipt_is_byte_compatible_with_clip_py`
  pins it.
- `session.jsonl` uses unreal-agent's codec
  (`{"type","data"}`; `session|item|operation`; contiguous `seq`; torn
  trailing line recoverable). Only the envelope shape is shared — record
  bodies are the seat's own v1, not Go's session taxonomy. Re-invoking with the same `request_id`
  observes/resumes instead of re-sending (at-most-once). Operation
  states are `ready|awaiting|canceling|completed|failed|canceled`;
  transitions are validated, the `result` item precedes the terminal
  state, and awaiting-without-run-identity fails closed. Retries that do
  new work (drain `busy`/`stale` retries) must use a new `request_id`;
  same-request re-invoke is crash recovery only. Delivered control ids
  are recorded so inbox dedup survives restarts.
- Retry is pre-start-only: 2s–30s backoff over ≤5 attempts with 20%
  jitter, honoring `retry_after`. In-seat retry covers only
  `Upstream|Internal|transport|Timeout`; `RateLimited` reports retryable
  but becomes `busy` for the drain's wait instead of an in-seat sleep.
  Mapping: `Unauthenticated |
  PermissionDenied → bounced`; `Validation → bounced` (+ `next_model`
  hint, Python side);   `RateLimited | AgentBusy → busy` (+
  `retry_after_ms`); `Upstream | Internal | transport | Timeout` retried;
  `NotFound | InvalidState | Cancelled → stale`; `Unknown` keeps raw
  code and goes to Jev triage Choice only.
