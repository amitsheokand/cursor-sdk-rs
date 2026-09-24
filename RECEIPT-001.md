# RECEIPT-001 — T-seat-toolgate

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

- `run_bounded` uses shell-like escape via synthetic `command` string.
- `edit_diff` rejects paths outside `fence` before calling toolgate.
- Unit test `run_bounded_writing_into_protected_root_is_caught` in `fence.rs`.

## 5. Measurement

- `SeatResult.tool_stats: {name: {calls, result_chars}}` from completed stream `tool_call` events.
- Documented in `PROTOCOL.md`.

## 6. Replace mode

- Adds `read`, `edit`, `write`, `shell` to `disallowed_tools` (keeps `task`).
- On `create_agent` failure after replace, reverts those entries, emits `status` warning, retries (add fallback).

## Deviations

- `TOOLGATE_TOOLS` lives in `toolgate.rs` (not appended to `jev::SEAT_TOOLS` array); prune/register paths merge both lists.
- Replace SDK fallback not covered by FakeBridge (no simulated rejection); logic is in `run.rs`.
- Workspace `cargo fmt --check` required a one-time `cargo fmt` on the full tree (pre-existing drift on several non-fence files); only fence paths are in the commit.

## Gate tails

### `nix develop -c cargo test -p cursor-seat`

```
test result: ok. 108 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### `nix develop -c cargo fmt --check`

```
(no output, exit 0)
```

### `nix build .#cursor-seat`

```
building '/nix/store/hrp4gjsm4icjwr4jf4fycmzppl7887yi-cursor-seat-0.1.0.drv'...
(exit 0, result symlink at ./result)
```
