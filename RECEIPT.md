sdkrun_v1

## Final review round (bd4e129 follow-up)

| Item | Status |
|------|--------|
| A. R/C drift: destination + origin | DONE |
| B. Component-wise symlink+`..` resolution | DONE |
| C. Snapshot records symlinks (target hash) | DONE |
| D. Missing test coverage (path keys, snapshot skips, drift events) | DONE |

## Files changed

- `seat/src/fence.rs`
- `seat/tests/fence_run.rs`
- `RECEIPT.md`

## cargo test -p cursor-seat

Two consecutive runs: **115 passed** each (73 lib + 42 integration/doc).

Run 1 last lines: see commit log / local `/tmp/cargo-r1.txt`
Run 2 last lines: see `/tmp/cargo-r2.txt`
