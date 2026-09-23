sdkrun_v1

## Files changed

- `seat/src/fence.rs` (new)
- `seat/src/lib.rs`
- `seat/src/main.rs`
- `seat/src/protocol.rs`
- `seat/src/run.rs`
- `seat/src/context.rs`
- `seat/PROTOCOL.md`
- `seat/tests/fence_run.rs` (new)
- `seat/tests/support/mod.rs`
- `seat/tests/seat_run.rs`
- `seat/tests/protocol_golden.rs`
- `seat/tests/jev_run.rs`
- `seat/tests/session_run.rs`

## New tests

- `edit_outside_cwd_bounces_and_cancels`
- `shell_cwd_outside_bounces_and_cancels`
- `read_outside_cwd_is_allowed`
- `protected_root_change_bounces`
- `out_of_fence_git_drift_gets_one_correction_then_fails`
- `in_fence_git_change_stays_ok`
- `request_without_fence_fields_round_trips`
- `fence::in_fence_file_and_dir_prefix`
- `fence::tool_escape_blocks_edit_outside_cwd`
- `fence::tool_escape_allows_read_outside`
- `fence::snapshot_detects_change`

## cargo test -p cursor-seat (last lines)

```
test result: ok. 97 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.20s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
