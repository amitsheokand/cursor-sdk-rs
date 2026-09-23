# RECEIPT — T-seat-fence-attribute

## Changes

- `seat/src/fence.rs`: `attribute`, `rebaseline_entries`, `containing_protected_root`, `is_regular_file` (after `walk_dir` / before `changed`); `worktree_file_agent_authored` + `git HEAD:<rel>` check on hash matches
- `seat/src/run.rs`: mutable `protected_baseline` + `fence_external_emitted` on `DriveState`; mid-run/post-drive attribution via `spawn_blocking`; `emit_fence_external`; `DriveFlags` carries baseline + dedup set
- `seat/PROTOCOL.md`: document `fence` kind `external` and worktree hash attribution rule (+ agent-authored via git HEAD)
- `seat/tests/fence_run.rs`: updated escape fixtures to copy worktree content; new external/attribution integration tests; `git_head_then_agent_edit` for git-backed worktrees

## Coordinator review

Equal hash match requires worktree file to differ from `git HEAD:<rel>` (or be new at HEAD); owner revert to HEAD while worktree clean → `external`, not escape; git failure fail-open to `external`.

## Tests

Unit (`seat/src/fence.rs`): `attribute_worktree_copy_is_escape`, `attribute_primary_revert_to_head_while_worktree_clean_is_external`, `attribute_new_untracked_worktree_copy_is_escape`, `attribute_different_content_is_external`, `attribute_missing_worktree_is_external`, `attribute_deletion_in_root_is_external`, `attribute_symlink_in_root_is_external` (unix)

Integration (`seat/tests/fence_run.rs`): `protected_root_midrun_external_edit_notices_once`, `protected_root_external_then_agent_copy_escapes`, `protected_root_post_drive_external_only`; updated `protected_root_midrun_escape_after_tool_call`, `protected_root_change_bounces`, `tool_call_completion_runs_protected_snapshot_when_due`

## Gate (tail)

```
   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
