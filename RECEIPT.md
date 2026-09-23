sdkrun_v1

## Review triage (7b41de9 follow-up)

| Item | Status |
|------|--------|
| Path resolution for non-existent paths / symlinks | ACCEPTED — implemented |
| Shell: all cwd keys; no command parse | ACCEPTED — implemented |
| path_from_args: all keys + nested `arguments` | ACCEPTED — implemented |
| Drift: porcelain=v1 -z parsing | ACCEPTED — implemented |
| Snapshot: no symlink walk, skip dirs, spawn_blocking | ACCEPTED — implemented |
| Emit `fence` drift events | ACCEPTED — implemented |
| attach post-drive fence + shared helper | ACCEPTED — implemented |
| MCP tool path classification | REJECTED — no change |
| Split fence on whitespace | REJECTED — no change |
| Fail-closed on git failure | REJECTED — no change |
| Patch-body parsing | REJECTED — no change |

## Files changed

- `seat/src/fence.rs`
- `seat/src/run.rs`
- `seat/PROTOCOL.md`
- `seat/tests/support/mod.rs` (unchanged this commit if only prior — include if touched)
- `seat/tests/fence_run.rs`
- `RECEIPT.md`

## cargo test -p cursor-seat (two consecutive runs)

Run 1: **111 passed** (69 lib + 42 integration/doc)

Run 2: **111 passed**

Last 10 lines run 1:
```
test ready_session_sends_normally_and_records ... ok
test live_run_attaches_instead_of_resending ... ok
test terminal_result_replays_with_no_bridge_contact ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Last 10 lines run 2:
```
test ready_session_sends_normally_and_records ... ok
test live_run_attaches_instead_of_resending ... ok
test terminal_result_replays_with_no_bridge_contact ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
