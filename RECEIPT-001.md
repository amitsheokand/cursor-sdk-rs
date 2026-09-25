# RECEIPT-001 — T-seat-test-tmp

## Helpers changed

- `seat/src/test_dir.rs` — `TestDir` RAII guard (`Deref` to `Path`, `Drop` → `remove_dir_all`, `fresh_in_temp`, `hold`, `fresh_under_home`).
- `seat/src/lib.rs` — `#[cfg(test)] pub mod test_dir` for unit tests.
- `seat/src/fence.rs` — `unique_temp` returns `TestDir`; HOME expansion tests use `fresh_under_home`.
- `seat/src/session.rs` — `tempdir` → `TestDir::hold`.
- `seat/src/protocol.rs`, `seat/src/clip.rs`, `seat/src/jev.rs`, `seat/src/toolgate.rs` — test dirs wrapped in `TestDir`; manual `remove_dir_all` removed.
- `seat/tests/support/mod.rs` — `TestDir` + `workspace_dir()` returns `TestDir`.
- `seat/tests/{fence_run,jev_run,session_run,seat_run,toolgate_run}.rs` — keep `TestDir` guards alive for the test body; request builders take `&Path` / `(TestDir, SeatRequest)` so dirs are not dropped early.

## Leak check

Baseline (before this change, same command from PACKET):

```
before=2 after=12
```

After this change (final gate run):

```
before=125 after=127
```

The `after` count exceeds `before` by 2. New entries under `/tmp` were `nix-develop-*` and `nix-shell.*` from `nix develop` wrapping `cargo test`, not `cursor-seat-*` / `fence-*` / `shell-*` test trees. No new `~/.cursor-seat-*` or `~/cursor-seat-*` dirs appeared across the run.

## Gates (last lines)

`nix develop -c cargo test -q -p cursor-seat`:

```
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

`nix develop -c cargo fmt --check`:

```
(clean — no diff)
```
