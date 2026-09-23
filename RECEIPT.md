# RECEIPT — T-seat-shell-guard

## What changed

| File | Summary |
|------|---------|
| `seat/src/fence.rs` | Shell command token scan against `protected_roots`; `tool_escape` takes protected roots |
| `seat/src/run.rs` | `DriveFence` carries baseline snapshot; live + heartbeat mid-run re-check |
| `seat/PROTOCOL.md` | Document shell command scan and mid-run protected-root re-check |
| `seat/tests/fence_run.rs` | Integration tests for shell command escape and mid-run snapshot escape |

## New tests

### `fence::tests` (unit)

- `shell_command_cp_into_protected_root`
- `shell_command_cd_into_protected_root`
- `shell_command_git_checkout_in_protected_root`
- `shell_command_cargo_target_dir_env`
- `shell_command_manifest_path_flag`
- `shell_command_home_spelling_variants`
- `shell_command_quoted_absolute_path`
- `shell_command_bash_lc_nested`
- `shell_command_allows_tmp_and_nix`
- `shell_command_prefix_not_string_substring`
- `shell_command_ignores_relative_symlink_into_root`
- `shell_command_path_under_cwd_sibling_of_root`

### `fence_run` (integration)

- `shell_command_into_protected_root_bounces_and_cancels`
- `protected_root_midrun_escape_after_tool_call`

## Gate (second run tail)

```
     Running tests/session_run.rs (target/debug/deps/session_run-b12077195ce0ebac)

running 3 tests
test live_run_attaches_instead_of_resending ... ok
test terminal_result_replays_with_no_bridge_contact ... ok
test ready_session_sends_normally_and_records ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
