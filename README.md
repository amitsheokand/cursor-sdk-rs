# cursor-sdk-rs

A Rust SDK for [Cursor](https://cursor.com) agents.

Drive Cursor agents from Rust — local agents that work on a directory on this
machine, and cloud agents that work on a git repository. It talks to the
`cursor-sdk-bridge` process over its stable `sdk.v1` Connect/protobuf contract,
and the bridge is an implementation detail: it gets found, launched,
handshaken, and stopped for you.

```rust
use cursor_sdk::{AgentOptions, Client};

let client = Client::new();
let answer = client
    .prompt(AgentOptions::local(".").model("composer-2.5"), "What does this repo do?")
    .await?;
println!("{answer}");
client.close().await?;
```

## Install

```toml
[dependencies]
cursor-sdk-rs = "1.0.31"
tokio = { version = "1", features = ["full"] }
```

The version tracks the `sdk.v1` contract it was generated from, so
`cursor-sdk-rs 1.0.31` vendors the protos from `cursor/sdk-bridge` tag `v1.0.31`.
Building needs no `protoc`: codegen runs through the pure-Rust
[`protox`](https://crates.io/crates/protox) compiler in `build.rs`.

You also need two things at runtime:

1. **A Cursor API key** from the [dashboard](https://cursor.com/dashboard/api).
   Export it as `CURSOR_API_KEY`, or pass
   `Client::builder().api_key(..)`. User keys and service-account keys both
   work; Team Admin keys do not.
2. **The `cursor-sdk-bridge` executable.** The shortest path:

   ```bash
   cargo install cursor-sdk-bridge-fetch
   cursor-sdk-bridge-fetch
   ```

   It detects your platform, downloads the matching archive, verifies its
   SHA-256 against a checksum compiled into the tool, and installs to
   `~/.cursor/sdk-bridge/` — a location this crate already searches.

   Alternatively `pip install cursor-sdk` puts one on `PATH`, or download the
   standalone archive from
   [`cursor/sdk-bridge` releases](https://github.com/cursor/sdk-bridge/releases)
   and point `CURSOR_SDK_BRIDGE_BIN` at `bin/cursor-sdk-bridge`.

   The fetcher is a **separate crate on purpose**: downloading needs TLS and
   archive handling (~59 extra dependencies), and keeping it out of the library
   means programs that provision the bridge another way never compile any of
   it. `cursor-sdk-rs` itself has no TLS dependency — the only socket it opens is
   loopback plaintext.

Verify the pieces line up without spending anything:

```bash
cargo run --example catalog
```

## Streaming a turn

`Agent::send` returns as soon as the stream opens, so output appears while the
agent is still working.

```rust
use cursor_sdk::{AgentOptions, Client, RunEvent};

let client = Client::new();
let agent = client
    .create_agent(AgentOptions::local("/path/to/repo").model("composer-2.5"))
    .await?;

let mut run = agent.send("Add a test for the parser.").await?;
while let Some(event) = run.next_event().await {
    match event? {
        RunEvent::Message(message) => match message.kind.as_str() {
            "assistant" => if let Some(text) = message.text() { print!("{text}") },
            "tool_call" => eprintln!("[{}]", message.payload["name"]),
            _ => {}
        },
        RunEvent::Completed(outcome) => println!("\n{}", outcome.status),
        _ => {}
    }
}
```

Only want the text? `run.next_text()` yields assistant output and skips
everything else. Don't want the stream at all? `agent.ask(prompt)` returns the
final answer, and `run.wait()` returns the full outcome.

A `Run` is also a `futures` stream via `run.into_stream()`.

## Runs survive dropped connections

Dropping a stream does not cancel the run — it keeps executing on the bridge.

- `run.resume()` reconnects to the durable event log.
- `run.wait()` falls back to `WaitLiveRun` if the stream dies first.
- `client.observe_run(run_id, after_offset)` replays a run from scratch or from
  a resume point.

The offset rules from the bridge's `docs/streaming.md` are enforced for you: a
live `Send` offset is never passed to `ObserveRun`, because the two use
different numbering and mixing them silently skips events. `run.resume()` on a
live stream therefore replays from the beginning; de-duplicate on your side.

## Errors are classified, not stringly typed

Every failure is an `Error`. Failed RPCs carry the bridge's structured
`SdkErrorDetails`, mapped onto a stable `ErrorKind`.

```rust
use cursor_sdk::ErrorKind;

match agent.send(prompt).await {
    Ok(run) => { /* ... */ }
    Err(error) if error.kind() == Some(ErrorKind::AgentBusy) => {
        tokio::time::sleep(error.retry_after().unwrap_or(ONE_SECOND)).await;
    }
    Err(error) if error.is_auth() => eprintln!("check CURSOR_API_KEY"),
    Err(error) => eprintln!("{error} (request {:?})", error.request_id()),
}
```

`request_id` is preserved in full — Cursor support traces requests by it, and a
truncated id is not traceable. `retry_after` and `rate_limit` survive too, and
`error.is_retryable()` answers the common question directly.

A run that *fails* is not an RPC error: it ends with a successful stream whose
outcome has a non-`Finished` status. Because `RunStreamResult.error_code` is
often empty, `RunOutcome::failure_reason()` falls back to the last `status`
message, which is where the human-readable reason actually arrives.

## Custom tools

Register a Rust function and the agent can call it. The SDK runs a loopback
Connect server; the bridge authenticates to it with a token this process chose.

```rust
use cursor_sdk::{Client, CustomTool};
use serde_json::json;

let client = Client::builder()
    .register_tool(
        CustomTool::new(
            "deployment_status",
            "Look up the deployment status of one of our services.",
            json!({
                "type": "object",
                "properties": {"service": {"type": "string"}},
                "required": ["service"],
            }),
        ),
        |call| async move {
            let service = call.require("service")?.as_str().unwrap_or_default().to_string();
            Ok(json!({"service": service, "version": "2026.9.3", "healthy": true}))
        },
    )
    .build();
```

Declaring the tool and implementing it are one step: the declaration is merged
into every local agent this client creates. A scalar return value is wrapped as
`{"value": ...}` automatically, because a tool result is a protobuf `Struct` and
can only encode an object.

Custom tools are a local-agent feature. See `examples/custom_tools.rs`.

## Custom agent stores

Implement `AgentStore` and the bridge stops persisting local agent state
itself, forwarding every operation to your code instead.

```rust
let client = Client::builder()
    .agent_store(LoggingStore::new(MemoryStore::new()))
    .build();
```

A store can only be configured *before* the bridge launches, because agents may
load state before any RPC arrives — so it lives on the builder, not on the
client. `MemoryStore` is a working implementation; `LoggingStore` traces every
operation, which is how you confirm shapes against your own bridge. See
`examples/custom_store.rs`.

## Bridge lifecycle

One managed bridge per client, created lazily on first use. A `Client` that is
built but never used spawns nothing.

```rust
let client = Client::builder()
    .workspace("/path/to/repo")          // --workspace
    .bridge_binary("/opt/cursor-sdk-bridge/bin/cursor-sdk-bridge")
    .verbose(true)                       // log every RPC to stderr → tracing
    .startup_timeout(Duration::from_secs(30))
    .build();
```

`client.close()` shuts the bridge down gracefully: `Shutdown`, then wait, then
kill. Dropping the last `Client` clone without closing — including on a panic
unwind — still kills the process, so a bridge cannot outlive the program on any
normal path. Signals are the exception, which `cursor_sdk::install_exit_guard()`
closes for binaries that want it.

To share a bridge somebody else runs:

```rust
let client = Client::builder().endpoint(url, token).build();
```

An attached client never stops that process.

Diagnostics go through [`tracing`]: `cursor_sdk::bridge` carries the bridge's
own stderr, `cursor_sdk::callback` the tool callbacks, `cursor_sdk::store` the
store traffic. The bearer token is never logged, and neither is the raw
discovery line (older bridges inline the token in it).

## What's covered

Every RPC in the contract, through plain Rust types:

| Area | API |
| --- | --- |
| Agents | `create_agent`, `create_agent_idempotent`, `resume_agent`, `agent`, `get_agent`, `list_agents`, `list_all_agents` |
| Agent lifecycle | `close`, `reload`, `archive`, `unarchive`, `delete` |
| Turns | `send`, `send_with`, `send_idempotent`, `ask` |
| Runs | `next_event`, `next_text`, `into_stream`, `wait`, `text`, `resume`, `cancel`, `observe_run`, `get_run`, `wait_live_run`, `runs`, `conversation` |
| Messages | `messages` |
| Artifacts | `artifacts`, `download_artifact`, `download_artifact_to` |
| Usage | `usage`, `run_usage` |
| Catalog | `me`, `models`, `repositories` |
| Bridge control | `ping`, `version`, `close`, `bridge_info`, `endpoint` |
| Callbacks | `ToolRegistry`, `AgentStore` |
| Escape hatch | `call_unary`, `call_server_stream` for anything a newer bridge adds |

Agent options cover both runtimes: models and model parameters, MCP servers
(stdio and HTTP), sub-agents, plan mode, built-in tool allow/deny lists,
sandboxing, setting sources, multi-root workspaces, cloud environments,
repositories and starting refs, PR automation, environment variables, and
metadata.

## Verification

Tested against the real `cursor-sdk-bridge` v1.0.31 on macOS:

- Spawn, ready-line handshake, token file, `Ping`, `GetVersion` (10
  capabilities), graceful shutdown, no orphan process.
- A full local agent turn on `composer-2.5`: streamed `thinking`, `assistant`,
  and terminal result with token usage.
- A custom tool invoked by the model through the loopback callback server.
- A custom store serving 77 operations across `agents`, `runs`, `runEvents`,
  and `checkpoints` for one real turn, with no errors.
- `cursor-sdk-bridge-fetch` installing a real release: 25.2 MiB downloaded,
  SHA-256 verified, extracted, and then found by the library's discovery chain
  with no environment variable set.
- The manifest preflight refusing a binary whose `manifest.json` was edited to
  claim the wrong platform, before spawning it.
- `agents.list`, `runs.list`, and `runEvents.list` round-tripping through the
  store envelope: the listed agents and runs arrive back through the public
  API, and replaying the run re-delivers all 9 events — which also confirms the
  `offset` values the store assigns.

Plus 155 automated tests, including an in-process stand-in for the bridge that
speaks real Connect framing — covering keepalives, unknown envelope cases,
stream errors, resume semantics, the error taxonomy, and process-lifecycle
failure paths. A separate compile-only suite (`tests/doc_surface.rs`) type-checks
every API the documentation claims exists, so an example cannot drift away from
the crate without `cargo test` noticing. The bridge-dependent tests skip
themselves when no binary is present:

```bash
cargo test                                                  # everything that needs no bridge
CURSOR_SDK_BRIDGE_BIN=/path/to/bin/cursor-sdk-bridge cargo test
```

Every custom-store shape is verified, `list` included. A plain turn never
issues one, so `examples/store_list_probe.rs` forces them and checks the
records survive the round trip. The envelope is `{"items": [...]}` with a
`nextCursor` — or `nextOffset` for run events, which resume by offset rather
than by an opaque cursor — present only when another page exists. Confirmed
against the bridge's own reference stores and live for `agents`, `runs`, and
`runEvents`; the `checkpoints` list (which returns blob ids, not records) is
confirmed against the reference implementation only.

## Examples

| Example | Shows |
| --- | --- |
| `catalog` | Identity, models, repositories. Starts no agent, spends nothing. |
| `quickstart` | One prompt, one answer. |
| `streaming` | Consuming a turn event by event. |
| `grok_fast` | Model parameters: Grok 4.6 with `fast=true` and `effort=low`, validated against the catalog. |
| `custom_tools` | Rust functions the agent can call. |
| `custom_store` | Owning durable agent state in-process. |
| `cloud_agent` | Cloud runtime, artifacts, billed usage. |
| `resume` | Surviving a dropped stream; continuing a conversation later. |
| `store_list_probe` | Forces a store `list` and prints the exact wire shapes. |

```bash
export CURSOR_API_KEY=...
cargo run --example streaming -- "Summarize this repository."
```

## Layout

```text
build.rs                 codegen via protox — no protoc needed
proto/sdk/v1/            the vendored sdk.v1 contract (generated; do not edit)
src/
  client.rs              bridge lifecycle, transport ownership, every RPC
  agent.rs               the Agent handle
  run.rs                 the Run handle and stream semantics
  options.rs             agent and send option builders
  types.rs               the public vocabulary
  error.rs               the error taxonomy
  bridge.rs              find, spawn, handshake, stop
  manifest.rs            the archive manifest, and the pre-spawn check on it
  transport.rs           Connect over HTTP/1.1 (unary + server streams)
  callback/              the loopback servers the bridge calls back into
  proto.rs, json.rs      the wire layer and Struct ↔ JSON
tests/
  protocol.rs            end-to-end against an in-process fake bridge
  lifecycle.rs           process lifecycle, including failure paths
  doc_surface.rs         compile-check of every documented API
bridge-fetch/            the installer, a separate crate so the library
                         never compiles a TLS stack
```

`proto/` is copied verbatim from a `cursor/sdk-bridge` release and must not be
edited. To move to a newer contract, replace `proto/sdk/v1/` and
`proto/manifest.json` from the new tag and rebuild; `sdk.v1` only changes
additively, so existing code keeps working and new RPCs stay invisible until
they are wrapped.

## Scope

Cursor publishes and supports the `sdk.v1` protos and the bridge binaries.
This crate is a community adapter built on that contract, not a first-party
SDK — versioning, support, and security review for it live here, not with
Cursor. If you write TypeScript or Python, use
[`@cursor/sdk`](https://www.npmjs.com/package/@cursor/sdk) or
[`cursor-sdk`](https://pypi.org/project/cursor-sdk/) instead.

SDK runs follow the same pricing, request pools, and Privacy Mode rules as the
IDE and Cloud Agents. Spend appears on the
[usage dashboard](https://cursor.com/dashboard/usage) under the SDK tag.

## License

MIT.

[`tracing`]: https://crates.io/crates/tracing
