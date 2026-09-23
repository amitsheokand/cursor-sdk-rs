# RECEIPT — T-seat-fence-attribute

## Review round 2

| Finding | Fix |
|--------|-----|
| Porcelain paths vs protected root | `rev-parse --show-toplevel/--show-prefix`; status from toplevel with `:(literal)<prefix><rel>` |
| Git env | `env_clear()` then PATH, HOME, `LC_ALL=C`, `GIT_OPTIONAL_LOCKS=0` |
| Timeout / deadlock | Own `Child`, `process_group(0)`, drain stdout/stderr, `kill -9 -<pgid>`, then wait |
| Post-drive undecided rebaseline | Emit `external`/`undecided` without rebaseline or dedup (self-check retries) |
| Glob metacharacters in paths | Literal pathspecs (round 2 test `attribute_literal_pathspec_with_glob_chars`) |
| Attach test timing | Silence across ≥1 debounce window on pre-attach baseline, then post-attach write |

Note: host `git status` (2.55) has no `--no-optional-locks` flag; optional locks suppressed via `GIT_OPTIONAL_LOCKS=0`.

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

## Tests (cumulative)

Unit: `attribute_escape_when_protected_root_is_subdirectory`, `attribute_owner_edit_in_subdirectory_repo_is_external`, `attribute_literal_pathspec_with_glob_chars` (+ round-1 attribute tests)

Integration: updated `attach_protected_baseline_only_reports_post_attach_changes`

## Gate (tail)

```
   Doc-tests cursor_seat

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
