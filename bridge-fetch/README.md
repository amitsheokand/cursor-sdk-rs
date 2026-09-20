# cursor-sdk-bridge-fetch

Installs the `cursor-sdk-bridge` binary that the
[`cursor-sdk-rs`](https://crates.io/crates/cursor-sdk-rs) crate drives.

```bash
cargo install cursor-sdk-bridge-fetch
cursor-sdk-bridge-fetch
```

It detects your platform, downloads the matching standalone archive from
`cursor/sdk-bridge` releases, **verifies its SHA-256 against a checksum
compiled into this tool**, and installs it to `~/.cursor/sdk-bridge/` — which
`cursor-sdk-rs` already searches, so nothing else needs configuring.

This is a separate crate on purpose. Downloading needs TLS and archive
handling, roughly 59 extra dependencies; keeping it out of the library means
programs that provision the bridge another way (a Dockerfile, a CI step,
`pip install cursor-sdk`) never compile any of it.

## Usage

```text
cursor-sdk-bridge-fetch [OPTIONS]

  --version <TAG>    Bridge release to install (default: 1.0.31)
  --dest <DIR>       Install root (default: ~/.cursor/sdk-bridge)
  --force            Reinstall even when a bridge is already present
  --print-path       Print the executable path and exit without downloading
  --list             List the releases and platforms this tool has checksums for
  --allow-unpinned   Install a release this tool has no pinned checksum for
  --quiet            Only print errors
  -h, --help         Print this help
```

## Security

Checksums are compiled into the binary, not fetched at runtime. Fetching
`SHA256SUMS.txt` from the same release it is validating would only detect a
truncated download — it travels over the same connection from the same origin
as the archive. A checksum pinned in reviewed, versioned source is the only
form that also detects a swapped asset.

The cost of that choice is a release cadence: a bridge version with no pinned
checksum here needs `--allow-unpinned`, which prints a prominent warning and
verifies nothing.

## License

MIT.
