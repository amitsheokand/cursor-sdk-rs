# RECEIPT-001 — T-flakes (tree B: cursor-sdk-rs)

## Built

- `nix/cursor-sdk-bridge.nix`: fetch `cursor/sdk-bridge` **v1.0.31** standalone tarballs per system; install under `$out/share/cursor-sdk-bridge`, symlink `$out/bin/cursor-sdk-bridge`; `dontStrip` / `dontPatchELF` / `dontFixup` (Bun compile ELF — no patchelf).
- `nix/cursor-seat.nix`: workspace `buildRustPackage` with `-p cursor-seat`, `wrapProgram` `--set-default CURSOR_SDK_BRIDGE_BIN` to packaged bridge; comment for future `cargoLock.outputHashes` if toolgate becomes a git dep.
- `flake.nix`: both packages on overlay, `default = cursor-seat`, devShell exports `CURSOR_SDK_BRIDGE_BIN`, cheap `checks.nix-files`, nixfmt formatter.
- `Cargo.lock`: vendored for reproducible Nix build (was missing from tree before flake).
- `README.md`: short Nix section (nix-ld / unpatchable bridge).

## Native inputs (why)

| Package | Inputs | Why |
|---------|--------|-----|
| `cursor-sdk-bridge` | none (fetch only) | prebuilt binary; no build |
| `cursor-seat` | `makeWrapper` | `postInstall` wrapper for bridge default |
| `cursor-seat` (check) | `git` | fence/git tests init repos in `preCheck` with writable `HOME=$TMPDIR` |

No `protoc`: `build.rs` uses prost-build + protox (verified by successful `nix build`).

## Gates (packet)

```text
$ nix build .#cursor-seat -L
cursor-seat>     Finished `release` profile [optimized] target(s) in 32.35s
cursor-seat> buildPhase completed in 33 seconds
cursor-seat> stripping (with command strip and flags -S -p) in .../bin

$ nix build .#cursor-sdk-bridge -L
(cursor-sdk-bridge derivation: fetch + tar install; no patchelf/strip fixup)

$ ./result/bin/cursor-sdk-bridge --help
Usage: cursor-sdk-bridge [options]
...
  --help, -h                            Show this help
exit=0

$ nix develop -c cargo test -q -p cursor-seat
test result: ok. 7 passed; 0 failed; ...
test result: ok. 11 passed; 0 failed; ...
test result: ok. 10 passed; 0 failed; ...
test result: ok. 3 passed; 0 failed; ...
```

## Review round 1

Grok NO-GO: `doCheck` off and `checks` only tested nix file existence. Fixed: `checks.<system>.cursor-seat` builds the package (check phase runs `cargo test -p cursor-seat`); `cleanSourceWith` on `lib.cleanSource` excludes `result` / `result-*`; sandbox skips only git- or `$HOME`-dependent fence tests (documented in `nix/cursor-seat.nix`).

```text
$ nix flake check -L
checking derivation checks.x86_64-linux.cursor-seat...
derivation evaluated to /nix/store/pwyb5s63f6fwmsxs4irrwgwqs8g79vl3-cursor-seat-0.1.0.drv
all checks passed!

$ nix build .#cursor-seat -L
cursor-seat> checkPhase completed in 39 seconds
cursor-seat> test result: ok. 92 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out; finished in 0.05s
cursor-seat> test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out; finished in 0.01s
cursor-seat> test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.86s
cursor-seat> stripping (with command strip and flags -S -p) in .../bin
```

## Review round 2

Removed 24 `checkFlags` git/`$HOME` skips; `nativeCheckInputs = [ git ]` and `preCheck` sets `HOME=$TMPDIR`, `GIT_CONFIG_NOSYSTEM=1`, and throwaway `git config --global user.name` / `user.email`. No network-only skip (loopback integration tests pass in the Nix check sandbox).

```text
$ nix flake check -L
checking derivation checks.x86_64-linux.cursor-seat...
derivation evaluated to /nix/store/hyxv0v1v6js642ry9vrskd281jzzf8wi-cursor-seat-0.1.0.drv
all checks passed!

$ nix build .#cursor-seat -L
cursor-seat> test result: ok. 105 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
cursor-seat> test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.13s
cursor-seat> test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cursor-seat> test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cursor-seat> test result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cursor-seat> test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.86s
cursor-seat> test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
cursor-seat> checkPhase completed in 52 seconds
cursor-seat> stripping (with command strip and flags -S -p) in .../bin
```
