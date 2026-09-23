# RECEIPT — T-seat-shell-guard

## What changed (565b3d4 + review round 1)

| File | Summary |
|------|---------|
| `seat/src/fence.rs` | Shell scan: quotes, relative paths, PATH `:`, nested cwd |
| `seat/src/run.rs` | Escape latch, tool_call dedup, shared snapshot debounce |
| `seat/PROTOCOL.md` | Shell command scan + mid-run re-check (unchanged this round) |
| `seat/tests/fence_run.rs` | Mid-run, latch, nested cwd, relative, PATH, quoted root |
| `seat/tests/support/mod.rs` | `StreamTimed` for delayed stream frames in tests |
| `seat/Cargo.toml` | `futures-util`, `tokio-stream` dev-deps for timed streams |

## Review round 1

| Finding | Fix | Test |
|---------|-----|------|
| Escape latch / duplicate cancel+event | `emit_fence_escape` no-ops when `fence_escape` set; `MissedTickBehavior::Skip` | `fence_escape_latch_single_cancel_and_event` |
| Replay re-triggers fence | `tool_call_is_replay` before fence check | (resume dedup path; latch test) |
| Snapshot cadence | `maybe_protected_snapshot_escape` + shared `last_protected_snapshot` ≥1s; skip when latched; seed clock at drive start | `protected_root_midrun_escape_after_tool_call` |
| Relative shell tokens | Join to effective shell cwd, lexical normalize, `hits_protected_root` | `shell_command_relative_dotdot_into_protected_root`, `shell_relative_command_into_protected_root_bounces` |
| Nested shell cwd keys | `collect_shell_cwd_values` reads `arguments` | `shell_nested_arguments_working_directory_escape`, `shell_nested_working_directory_into_root_bounces` |
| Tokenizer / PATH colon | Quoted spans, `\` escape, inner re-tokenize, `=` value split on `:` | `shell_command_path_colon_field_hits_root`, `shell_path_colon_field_into_root_bounces`, `shell_quoted_path_with_space_in_root_name` |
| JoinError → empty digest | `snapshot_async` returns `None` on join failure | (mid-run skips sample; no false escape) |
| Mid-run test race | Write on `RunStarted` before delayed `tool_call`; `StreamTimed` | `protected_root_midrun_escape_after_tool_call` |

## New tests (round 1)

### Unit (`fence::tests`)

- `shell_command_relative_dotdot_into_protected_root`
- `shell_command_ln_relative_into_root`
- `shell_command_path_colon_field_hits_root`
- `shell_nested_arguments_working_directory_escape`
- `shell_quoted_path_with_space_in_root_name`

### Integration (`fence_run`)

- `shell_nested_working_directory_into_root_bounces`
- `shell_relative_command_into_protected_root_bounces`
- `shell_quoted_space_root_name_bounces`
- `shell_path_colon_field_into_root_bounces`
- `fence_escape_latch_single_cancel_and_event`

## Review round 2

| Finding | Fix | Test |
|---------|-----|------|
| `call_id` dedup dropped completion frames | `fence_checked_tool_calls` skips only `tool_fence_hit`; `emit_message` dedupes `{call_id}:{status}`; snapshot after every tool_call | `tool_call_started_and_completed_both_emit`, `tool_call_completion_runs_protected_snapshot_when_due` |
| `cd`/`pushd` lexical base | Left-to-right walk updates base; `cd` targets checked for escape | `shell_cd_updates_lexical_base_for_later_tokens` |
| PROTOCOL relative + cd rules | Documented in fence section | (docs only) |

## New tests (round 2)

- `shell_cd_updates_lexical_base_for_later_tokens` (unit)
- `tool_call_started_and_completed_both_emit` (integration)
- `tool_call_completion_runs_protected_snapshot_when_due` (integration)

## Gate (second run tail)

```
     Running tests/session_run.rs (target/debug/deps/session_run-97c18104d41e045e)

running 3 tests
test live_run_attaches_instead_of_resending ... ok
test terminal_result_replays_with_no_bridge_contact ... ok
test ready_session_sends_normally_and_records ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
