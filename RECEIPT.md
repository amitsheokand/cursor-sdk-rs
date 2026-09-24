# RECEIPT — T-seat-toolgate

## 1. Dependency

- `seat/Cargo.toml`: git `toolgate` at rev `79b0e8c17abab48bf8d7f54afdbabda72993b9d4`, `default-features = false`.
- `Cargo.lock`: toolgate git source entry.
- `nix/cursor-seat.nix`: `cargoLock.outputHashes."toolgate-0.1.0" = "sha256-I9v/kCH/e49lKay6MjPfLtY5nwj2yCPiRiwBDn6DtNY="`.
- `rust-version` bumped **1.82 → 1.85** (toolgate edition 2024 / MSRV).

## 2. Request config

- `SeatRequest.toolgate: ToolgateConfig` (`mode`: off|add|replace, optional `gates`), `deny_unknown_fields`.
- Golden test `seat_request_toolgate_defaults_and_rejects_unknown` in `protocol_golden.rs`.
- `PROTOCOL.md` documents `toolgate` on stdin.

## 3. Tools (`seat/src/toolgate.rs`)

- `read_window`, `edit_diff`, `run_bounded`, `run_gates` (when `gates` non-empty).
- Registered from `run_seat` when mode is active; included in prune candidates and `tools_enabled` union.
- Handlers use `spawn_blocking` + `{"error": ...}` on failure.

## 4. Fence

- `run_bounded` runs `tool_escape` on real argv before `toolgate::run` (protected roots via `ToolgateContext`).
- `edit_diff` rejects paths outside `fence` before calling toolgate.
- `run_argv_protected_escape` in `fence.rs` (no space-joined synthetic shell command).
- Unit/integration tests for protected-root block and fence paths.

## 5. Measurement

- `SeatResult.tool_stats: {name: {calls, result_chars}}` from completed `tool_call` events.
- Built-in tools: prefer `result.text` char count; seat custom tools: `{"result": …}` wire size (SDK callback JSON).
- Documented in `PROTOCOL.md`.

## 6. Replace mode

- Adds `read`, `edit`, `write`, `shell` to `disallowed_tools` (keeps `task`); tracks names added for selective revert.
- On `CreateAgent` **disallowed-tool** validation failure only: revert added names, warning, retry without replace disallows.

## Review round 1 (grok NO-GO)

| Finding | Fix | Test |
|--------|-----|------|
| `run_bounded` ran before fence; space-joined command ≠ argv | `tool_escape` in `run_bounded` before `run`; `run_argv_protected_escape` | `run_bounded_into_protected_root_returns_error_before_run` |
| Any `create_agent` error triggered replace→add; revert dropped pre-existing disallows | `create_agent_disallowed_tool_rejection`; `revert_replace_disallowed(request, added)` | `replace_falls_back_only_when_create_agent_rejects_disallowed_tool`, `replace_does_not_fallback_on_unrelated_create_agent_error`, `revert_replace_only_removes_names_this_attempt_added` |
| `tool_stats` counted serde JSON of stream `result`, not model wire text | Custom-tool callback wrap; built-in `text` field | `tool_stats_count_completed_tool_calls` (`5`), `tool_stats_counts_custom_tool_callback_wire_size` |

## Deviations

- `TOOLGATE_TOOLS` lives in `toolgate.rs` (not appended to `jev::SEAT_TOOLS` array); prune/register paths merge both lists.
- Workspace `cargo fmt` run for gate (`cargo fmt --check`); touches files outside the packet fence (same as round 1).

## Gate tails

### `nix develop -c cargo test -p cursor-seat`

```
test result: ok. 109 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### `nix develop -c cargo fmt --check`

```
(no output, exit 0)
```

### `nix build .#cursor-seat`

```
building '/nix/store/hgaxp10l9hrc1nibxjv5v5ga75q5xa0v-cursor-seat-0.1.0.drv'...
(exit 0)
```
