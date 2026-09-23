sdkrun_v1

## Files changed (review fixes)

- `seat/src/fence.rs`
- `seat/src/run.rs`
- `seat/src/protocol.rs`
- `seat/tests/support/mod.rs`
- `RECEIPT.md`

## New / updated tests

- `workspace_dir` uniqueness (pid + counter, remove before create)
- `tilde_fence_entry_under_cwd_matches`
- `fence_entry_strips_trailing_prose`
- `absolute_fence_outside_cwd_is_ignored`
- `fence_only_outside_entries_yields_no_drift`
- `snapshot_ignores_mtime_only_change`
- `snapshot_detects_content_change` (renamed from mtime-based)

## cargo test -p cursor-seat (two consecutive runs, last ~10 lines each)

Run 1:
```
test ready_session_sends_normally_and_records ... ok
test live_run_attaches_instead_of_resending ... ok
test terminal_result_replays_with_no_bridge_contact ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Run 2:
```
test live_run_attaches_instead_of_resending ... ok
test terminal_result_replays_with_no_bridge_contact ... ok
test ready_session_sends_normally_and_records ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

(Suite totals: 102 tests per run, all passed.)
