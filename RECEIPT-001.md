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
