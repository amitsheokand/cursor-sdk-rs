# RECEIPT — T-seat-fence-attribute

## Review round 1

| Finding | Fix |
|--------|-----|
| HEAD-match false escape on owner revert | Escape requires protected repo path dirty (`git status --porcelain` on root), not worktree vs HEAD |
| Git env pollution / hang risk | `git` with `GIT_*` cleared, `LC_ALL=C`, 10s timeout + kill |
| Git failure misclassified | Mid-run **undecided** (no event/rebaseline/dedup); post-drive `external` with `tool: "undecided"` |
| Snapshot/hash race | `snapshot_and_attribute` in one `spawn_blocking` pass |
| Self-check follow-up stale baseline | Pass `flags.protected_baseline` + dedup into follow-up `drive` + `apply_post_drive_fence` |
| Attach baseline scope undocumented | PROTOCOL.md + `attach_protected_baseline_only_reports_post_attach_changes` |
| External dedup test timing | Stream open ≥5s / multiple heartbeat windows before finish |
| Protected escape fixtures | Git repo on primary (`init_protected_fence_git`) for dirty detection |

## Changes (cumulative)

- `seat/src/fence.rs`: `AttributeResult`, `snapshot_and_attribute`, dirty-primary `attribute`, robust `git_status_dirty_rels`
- `seat/src/run.rs`: `apply_attribute_result`, undecided handling, self-check post-fence, `DriveFence.fence_external_emitted`
- `seat/PROTOCOL.md`: dirty-primary attribution, undecided, attach baseline
- `seat/tests/fence_run.rs`: integration + attach/git-failure tests

## Tests

Unit: `attribute_primary_revert_to_head_while_worktree_clean_is_external`, `attribute_owner_commit_same_bytes_in_primary_is_external`, `attribute_agent_commit_in_worktree_then_copy_is_escape`, `attribute_git_failure_is_undecided`, `attribute_rename_in_primary_is_external` (+ prior attribute tests)

Integration: `protected_root_git_failure_post_drive_emits_undecided`, `attach_protected_baseline_only_reports_post_attach_changes`; updated `protected_root_midrun_external_edit_notices_once`

## Gate (tail)

```
   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
