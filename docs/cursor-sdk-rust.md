# Cursor Rust SDK

The `cursor-sdk-rs` crate lets you call Cursor's agent from your own Rust code. The same agent that runs in the Cursor IDE, CLI, and web app is scriptable from Rust with async streams, typed structs, and an error taxonomy you match on instead of parse.

Unlike the [TypeScript](/docs/sdk/typescript) and [Python](/docs/sdk/python) SDKs, this is a community adapter built on the published [SDK Bridge](/docs/sdk/bridge) contract. Cursor publishes and supports the `sdk.v1` protos and the `cursor-sdk-bridge` binaries; versioning, support, and security review for this crate live with the crate. Everything below is the crate's own surface.

## Overview

The SDK wraps local and cloud runtimes behind one interface. You write the same code regardless of where the agent runs.

| Runtime | What it does | When to use |
| --- | --- | --- |
| **Local** | Runs the agent against local files on disk, next to the bridge process. | Dev scripts and CI checks against a working tree. |
| **Cloud (Cursor-hosted)** | Runs in an isolated VM with your repo cloned in. Cursor runs the VMs. | When the caller doesn't have the repo, you want many agents in parallel, or runs need to survive the caller disconnecting. |

Set the runtime by passing `local` or `cloud` to `AgentOptions`.

> **Local means local agent loop, not local model.** "Local" describes where the agent loop and filesystem access run, not where the model runs. All inference goes through Cursor's hosted models in both modes.

### How it differs from the first-party SDKs

The TypeScript SDK embeds the agent runtime in your Node process. This crate does not embed anything: it drives `cursor-sdk-bridge`, a local process that wraps the TypeScript SDK and exposes it over a stable Connect/protobuf contract. The bridge is an implementation detail — the crate finds it, launches it, handshakes with it, and stops it — but it explains two things you will notice:

- You need the `cursor-sdk-bridge` executable on the machine — `cargo install cursor-sdk-bridge-fetch && cursor-sdk-bridge-fetch`, or any of the other routes in [Installation](#installation). The library never downloads it for you, by design.
- Features that never reach the `sdk.v1` wire contract are not available here. See [Known limitations](#known-limitations).

## Authentication

Set `CURSOR_API_KEY` or pass `api_key` before creating an agent.

The SDK accepts user API keys and service account API keys for both local and cloud runs. Team Admin API keys are not yet supported.

- **User API key** from [Cursor Dashboard -> API Keys](https://cursor.com/dashboard/api)
- **Service account API key** from [Team settings](https://cursor.com/dashboard/team-settings). See [Service accounts](/docs/account/enterprise/service-accounts)

```bash
export CURSOR_API_KEY="your-key"
```

`Client::new()` reads `CURSOR_API_KEY` from the environment. The key is put in the bridge process's environment *and* set explicitly on every request that accepts one. That second half matters: not every bridge build falls back to the environment variable for every operation, and when it doesn't, the failure surfaces much later as `Invalid User API Key` on the first send. The crate always sets the option, so you never hit that.

Two separate secrets are in play, and only the first is yours to manage:

| Secret | Who makes it | Where it goes |
| --- | --- | --- |
| **Cursor API key** | You | The bridge environment, plus `api_key` on agent and catalog requests. |
| **Bridge bearer token** | The bridge, per process | Generated during the handshake and sent on every local RPC. The crate handles it; it is never logged. |

## Usage and billing

SDK runs follow the same pricing, request pools, and Privacy Mode rules as runs from the IDE and Cloud Agents. Spend shows up in your team's [usage dashboard](https://cursor.com/dashboard/usage) under the SDK tag.

Service account API keys bill to the team that owns the service account. User API keys bill to that user's plan.

To read per-run token counts in code, see [Token usage](#token-usage). To fetch billed usage and dollar cost for an agent's runs, see [`agent.usage()`](#agentusage).

## Core concepts

| Concept | Description |
| --- | --- |
| **`Client`** | Owns the bridge process and the transport. Cheap to clone (an `Arc` inside), and spawns nothing until first use. |
| **`Agent`** | Durable handle that holds conversation state, workspace config, and model selection. Survives across multiple prompts. |
| **`Run`** | One prompt submission. Owns its own stream, outcome, resume state, and cancellation. |
| **`RunEvent`** | A typed event yielded during a run. Same shape across local and cloud runtimes. |
| **`Error`** | Every failure, classified by [`ErrorKind`](#errors) rather than by message text. |

## Installation

```toml
[dependencies]
cursor-sdk-rs = "1.0.31"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

Requires Rust 1.82 or later and a Tokio runtime.

The crate version tracks the `sdk.v1` contract it was generated from: `cursor-sdk-rs 1.0.31` vendors the protos from `cursor/sdk-bridge` tag `v1.0.31`. Building needs no `protoc` — codegen runs through the pure-Rust [`protox`](https://crates.io/crates/protox) compiler in `build.rs`.

### Getting the bridge

You also need the `cursor-sdk-bridge` executable at runtime. The shortest path:

```bash
cargo install cursor-sdk-bridge-fetch
cursor-sdk-bridge-fetch
```

That detects your platform, downloads the matching archive, verifies its SHA-256 against a checksum compiled into the tool, and installs to `~/.cursor/sdk-bridge/` — which this crate already searches, so nothing else needs configuring.

It is a **separate crate on purpose.** Downloading needs a TLS stack and archive handling, roughly 59 extra dependencies. Keeping it out of the library means a program that provisions the bridge another way — a Dockerfile, a CI step, `pip install cursor-sdk` — never compiles any of it. The `cursor-sdk-rs` library itself has no TLS dependency at all, because the only socket it opens is loopback plaintext.

The other ways, all equally supported:

- `pip install cursor-sdk`, which puts a bridge on `PATH`; or
- download `cursor-sdk-bridge-standalone-<os>-<arch>.tar.gz` from [`cursor/sdk-bridge` releases](https://github.com/cursor/sdk-bridge/releases) and point `CURSOR_SDK_BRIDGE_BIN` at `bin/cursor-sdk-bridge`.

`<os>` is `linux`, `darwin`, or `win32`; `<arch>` is `x64` or `arm64` (win32 is `x64` only) — Node's vocabulary, not Rust's, so `x86_64` maps to `x64` and `macos` to `darwin`. Prefer a bridge whose release tag matches the crate version; older adapters keep working against newer bridges.

The crate looks in this order, and stops at the first override it finds:

1. `Client::builder().bridge_binary(path)`
2. `CURSOR_SDK_BRIDGE_BIN`
3. `cursor-sdk-bridge` on `PATH`
4. `~/.cursor/sdk-bridge/bin/cursor-sdk-bridge`

An override that points at a file that does not exist is an error, not a reason to fall through — silently launching a different bridge than you asked for would be worse than failing.

Confirm the binary before debugging anything else:

```bash
cursor-sdk-bridge --help
```

### The manifest preflight

A standalone archive ships a `manifest.json` beside the binary saying which platform it was built for and which contract it implements. Before spawning, the crate reads it and fails early on a mismatch that cannot work:

| Field | On mismatch |
| --- | --- |
| `protocol` | **Error.** A different contract is not something this crate can speak. |
| `os` | **Error.** A Linux binary cannot exec on macOS. |
| `arch` | Warning only. Rosetta and qemu make a cross-architecture bridge legitimately common. |
| `sdkVersion` | Debug log only. `sdk.v1` changes additively. |

The check is soft in both directions: a bridge with no manifest beside it — the `pip`-installed copy on `PATH`, for instance — is fine, and so is a manifest that fails to parse. Only a manifest that is present *and* says something disqualifying is an error. Set `CURSOR_SDK_BRIDGE_SKIP_MANIFEST_CHECK=1` to bypass it entirely.

You can read the manifest yourself with [`BridgeManifest::for_binary`](#bridgemanifest).

## Quick start

```rust
use cursor_sdk::{AgentOptions, Client};

#[tokio::main]
async fn main() -> cursor_sdk::Result<()> {
    let client = Client::new();

    let agent = client
        .create_agent(AgentOptions::local(".").model("composer-2.5"))
        .await?;

    println!("{}", agent.ask("Summarize what this repository does").await?);

    agent.close().await?;
    client.close().await?;
    Ok(())
}
```

[Stream events](#stream-events) shows how to extract assistant text, handle tool calls, and read run state. For a one-shot prompt (create, run, finish), see [`client.prompt()`](#clientprompt).

> **Quickstart approves tool calls automatically.** The default local agent runs tool calls (shell, edit, write, and so on) without asking for approval; there is no human-in-the-loop prompt in headless mode. To gate tool calls, configure [hooks](#hooks), enable [`sandbox(true)`](#sandbox-options), or enable [`auto_review(true)`](#auto-review).

### Cloud quick start

```rust
use cursor_sdk::{AgentOptions, Client, CloudAgent, CloudRepository};

#[tokio::main]
async fn main() -> cursor_sdk::Result<()> {
    let client = Client::new();

    let agent = client
        .create_agent(AgentOptions::new().cloud_options(
            CloudAgent::new("https://github.com/your-org/your-repo")
                .repositories([
                    CloudRepository::new("https://github.com/your-org/your-repo")
                        .starting_ref("main"),
                ])
                .auto_create_pr(true),
        ))
        .await?;

    println!("{}", agent.ask("Add structured logging to the auth middleware").await?);

    client.close().await?;
    Ok(())
}
```

Cloud agents started by the SDK are filtered out of the default agent list. To view them in Cursor Web or a Cursor agents window, click **Filter > Source > SDK**.

## Async and concurrency

Every I/O method on this crate is `async` and expects a Tokio runtime. There is no synchronous facade; wrap calls in `Handle::block_on` if you need one.

`Client` is `Clone` and cheap: cloning shares one bridge process, one connection pool, and one tool registry. Clone it into tasks rather than building a second client.

```rust
use cursor_sdk::{AgentOptions, Client};

let client = Client::new();

let mut tasks = Vec::new();
for path in ["/repo/a", "/repo/b", "/repo/c"] {
    let client = client.clone();
    tasks.push(tokio::spawn(async move {
        client
            .prompt(AgentOptions::local(path).model("composer-2.5"), "Any TODOs left?")
            .await
    }));
}

for task in tasks {
    println!("{}", task.await.unwrap()?);
}
client.close().await?;
# Ok::<(), cursor_sdk::Error>(())
```

The bridge is created lazily on the first call that needs it, so a `Client` you build and never use never starts a process. Concurrent first calls collapse into a single launch.

## Creating agents

`client.create_agent()` validates options, creates the agent, and returns a handle. Pass either a local or a cloud runtime.

```rust
use cursor_sdk::{AgentOptions, CloudAgent, CloudRepository, Client};

let client = Client::new();

// Local agent.
let agent = client
    .create_agent(AgentOptions::local("/path/to/repo").model("composer-2.5"))
    .await?;

// Cloud agent.
let cloud_agent = client
    .create_agent(AgentOptions::new().cloud_options(
        CloudAgent::new("https://github.com/your-org/your-repo").auto_create_pr(true),
    ))
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

`agent.id()` is populated immediately. Local agents get an `agent-<uuid>` ID; cloud agents get a `bc-<uuid>` ID. `agent.model()` returns the `ModelChoice` the bridge resolved.

Local agents require an explicit model. Omitting one is caught before anything reaches the wire:

```text
invalid configuration: local agents require an explicit model;
list the available ids with Client::models()
```

Cloud agents fall back to the server-resolved default.

### No-repo cloud agents

Cloud agents can run on an empty VM with no repository.

```rust
use cursor_sdk::{AgentOptions, CloudAgent, Client};

let client = Client::new();
let agent = client
    .create_agent(AgentOptions::new().cloud_options(CloudAgent::no_repository()))
    .await?;

println!("{}", agent.ask("Research the top 3 Rust web frameworks and summarize.").await?);
# Ok::<(), cursor_sdk::Error>(())
```

No-repo agents must be enabled for your account or team. Repository-scoped API keys can't create them; use an unrestricted service account key or a user API key instead.

### Session environment variables

For cloud agents, pass `env_var` when a run needs short-lived credentials or other values that should live only with that agent.

```rust
use cursor_sdk::{AgentOptions, CloudAgent};

let options = AgentOptions::new().cloud_options(
    CloudAgent::new("https://github.com/your-org/your-repo")
        .env_var("STAGING_API_TOKEN", std::env::var("STAGING_API_TOKEN").unwrap()),
);
```

These values are encrypted at rest, injected into the cloud agent's shell, and deleted with the agent. They can't be combined with a caller-supplied `agent_id`; omit it and read the server-minted ID from `agent.id()`. Variable names can't start with `CURSOR_`.

For values that should only exist during a single run, pass them on send instead. See [Per-run environment variables](#per-run-environment-variables).

### Agent metadata

Attach your own string tags to a cloud agent at creation. They are persisted with the agent and read back on `AgentRuntime::Cloud { metadata, .. }` from `get_agent()` and `list_agents()`. These tags are not the in-VM [agent metadata](/docs/cloud-agent/metadata) API.

```rust
use cursor_sdk::{AgentOptions, CloudAgent};

let options = AgentOptions::new().cloud_options(
    CloudAgent::new("https://github.com/your-org/your-repo")
        .metadata("end_user_id", "user-123")
        .metadata("ticket_id", "ENG-456"),
);
```

You can attach up to 50 key-value pairs. Keys must be non-empty and no more than 255 characters; values must be strings no larger than 4096 bytes. If metadata isn't enabled for the API key's account, creating an agent with a non-empty map fails with `ErrorKind::PermissionDenied`.

### Model parameters

Use `ModelChoice::with_param` to pass per-model options such as reasoning effort. Parameter ids and values vary by model. Use [`client.models()`](#the-catalog) to discover supported parameters and preset variants for your account.

```rust
use cursor_sdk::{AgentOptions, ModelChoice};

let options = AgentOptions::local(".")
    .model(ModelChoice::new("composer-2.5").with_param("fast", "true"));
```

Anywhere a model is accepted, a bare `&str` works too — `.model("composer-2.5")` is `ModelChoice::new("composer-2.5")`.

Two behaviors are worth knowing before you rely on a parameter:

- **An unrecognized parameter id is accepted and silently ignored.** Nothing rejects it and the run succeeds; you simply never get the setting you asked for. This is easy to hit because the "how hard should it think" control is named differently per family — `effort` on Grok, `reasoning` on the GPT models, `reasoning_effort` on Gemini Flash, `thinking` on Claude. Sending `thinking=true` to `grok-4.6` is a no-op, not an error.
- **Omitting a parameter is not neutral.** The run uses that parameter's first allowed value, which may not be the default you assumed.

Both are reasons to validate a selection against [`client.models()`](#the-catalog) rather than hard-code it. `examples/grok_fast.rs` shows the check:

```rust
let model = models.iter().find(|model| model.id == "grok-4.6").ok_or("unavailable")?;

let mut choice = model.choice();
for (id, value) in [("fast", "true"), ("effort", "low")] {
    let parameter = model
        .parameters
        .iter()
        .find(|parameter| parameter.id == id)
        .ok_or_else(|| format!("no {id:?} parameter on this model"))?;
    if !parameter.values.iter().any(|allowed| allowed.value == value) {
        return Err(format!("{id}={value:?} is not an allowed value").into());
    }
    choice = choice.with_param(id, value);
}
```

`outcome.model` after the run reports the selection that actually executed, which is where to confirm the parameters took effect.

> **Composer 2 reroutes to Composer 2.5.** Composer 2 is retired. Requests that still pass `composer-2` are rerouted to Composer 2.5 at auth time, so existing scripts keep working.

### Cursor Router

[Cursor Router](/docs/cursor-router) selects a model for each Auto request. In the SDK, Router is the `auto-smart` model with an `optimize_for` parameter. It is available on Teams and Enterprise. Enterprise admins must enable Router for the team before `auto-smart` appears in the catalog.

#### Select Cost, Balance, or Intelligence

Pass `auto-smart` and set `optimize_for` explicitly:

| Product label | SDK value |
| --- | --- |
| Cost | `cost` |
| Balance | `balanced` |
| Intelligence | `intelligence` |

Use **Balance** in product copy. Use `balanced` only as the SDK wire value.

```rust
use cursor_sdk::{AgentOptions, Client, ModelChoice};

let client = Client::new();
let agent = client
    .create_agent(
        AgentOptions::local(".")
            .model(ModelChoice::new("auto-smart").with_param("optimize_for", "balanced")),
    )
    .await?;

let outcome = agent.send("Find and fix the failing authentication test").await?.wait().await?;
println!("{}", outcome.status);
# Ok::<(), cursor_sdk::Error>(())
```

Always pass `optimize_for`. Do not omit it and do not send a legacy `default` value; discovery through the catalog is the supported contract.

#### Discover Router in the model catalog

Treat the catalog as the source of truth before hard-coding a selection:

```rust
use cursor_sdk::{Client, Error, ModelChoice};

let client = Client::new();
let models = client.models().await?;

let router = models
    .iter()
    .find(|model| model.id == "auto-smart")
    .ok_or_else(|| Error::Config(
        "Cursor Router is not available for this API key. \
         Verify that Router is enabled for the key's team.".into()
    ))?;

let optimize_for = router
    .parameters
    .iter()
    .find(|parameter| parameter.id == "optimize_for")
    .ok_or_else(|| Error::Config("Router has no optimize_for parameter".into()))?;

let requested = "balanced";
if !optimize_for.values.iter().any(|value| value.value == requested) {
    return Err(Error::Config(
        format!("Router mode {requested:?} is not enabled for this team"),
    ));
}

let model = ModelChoice::new(&router.id).with_param(&optimize_for.id, requested);
# Ok::<(), cursor_sdk::Error>(())
```

#### Switch modes per run

Override the model on send to change Router mode for a run:

```rust
use cursor_sdk::{ModelChoice, SendOptions};

let run = agent
    .send_with(
        "Handle this complex migration",
        SendOptions::new()
            .model(ModelChoice::new("auto-smart").with_param("optimize_for", "intelligence")),
    )
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

Per-run model overrides are sticky server-side: later sends without an override keep using the new selection. See [Per-run model override](#per-run-model-override).

#### Model ids: auto-smart, auto, and default

| Selection | Meaning |
| --- | --- |
| `auto-smart` with `optimize_for` | Cursor Router. Use this when you want Cost, Balance, or Intelligence. |
| `ModelChoice::new("auto")` | Server-selected Auto fallback when a specific model is missing from the catalog. |
| Omitting `optimize_for`, or sending `default` | Not a supported Router contract. Always discover allowed values and pass `cost`, `balanced`, or `intelligence`. |

#### Billing and routing pool

- All Auto modes bill at the list price of the model each request is routed to.
- The underlying model can change between requests. Prefer a fixed model id when you need reproducible comparisons.
- Enterprise model allowlists shape the routing pool. Blocking required models can disable Router.

### Agent

The handle returned by `client.create_agent()`, `client.resume_agent()`, and `client.agent()`.

```rust
impl Agent {
    pub fn id(&self) -> &str;
    pub fn model(&self) -> Option<&ModelChoice>;
    pub fn cwd(&self) -> Option<&Path>;
    pub fn client(&self) -> &Client;

    pub async fn send(&self, prompt: impl Into<Prompt>) -> Result<Run>;
    pub async fn send_with(&self, prompt: impl Into<Prompt>, options: SendOptions) -> Result<Run>;
    pub async fn send_idempotent(
        &self,
        prompt: impl Into<Prompt>,
        options: SendOptions,
        idempotency_key: impl Into<String>,
    ) -> Result<Run>;
    pub async fn ask(&self, prompt: impl Into<Prompt>) -> Result<String>;
    pub async fn observe(&self, run_id: impl Into<String>, after_offset: Option<&str>) -> Result<Run>;

    pub async fn close(&self) -> Result<()>;
    pub async fn reload(&self) -> Result<()>;
    pub async fn archive(&self) -> Result<()>;
    pub async fn unarchive(&self) -> Result<()>;
    pub async fn delete(&self) -> Result<()>;
    pub fn close_on_drop(&self) -> AgentGuard;

    pub async fn info(&self) -> Result<AgentInfo>;
    pub async fn runs(&self, filter: ListRuns) -> Result<Page<RunOutcome>>;
    pub async fn messages(&self, filter: ListMessages) -> Result<Vec<AgentMessage>>;
    pub async fn conversation(&self, run_id: impl Into<String>) -> Result<serde_json::Value>;
    pub async fn cancel_run(&self, run_id: &str) -> Result<()>;

    pub async fn artifacts(&self) -> Result<Vec<Artifact>>;
    pub async fn download_artifact(&self, path: &str) -> Result<Vec<u8>>;
    pub async fn download_artifact_to(&self, path: &str, destination: impl AsRef<Path>) -> Result<u64>;
    pub async fn usage(&self) -> Result<AgentUsage>;
    pub async fn run_usage(&self, run_id: &str) -> Result<AgentUsage>;
}
```

| Member | Description |
| --- | --- |
| `id` | Stable agent identifier. `agent-<uuid>` for local, `bc-<uuid>` for cloud. |
| `model` | Model selection the bridge resolved at creation. `None` on a handle built with `client.agent(id)`. |
| `cwd` | Working directory this agent was created with. Several RPCs route by it, and the handle passes it for you. |
| `send` | Start a new run. Returns as soon as the stream opens. |
| `ask` | Send and wait for the final assistant text. |
| `close` | Release the agent's local resources. Durable state survives. |
| `reload` | Re-read filesystem config (hooks, project MCP, subagents) without closing. |
| `close_on_drop` | An RAII guard that closes the agent when it goes out of scope, including on the `?` path. |
| `artifacts` / `download_artifact` | Files produced by the agent (cloud only; local returns empty). |
| `usage` | Billed token usage and dollar cost (cloud only). |

`Agent` is `Clone`, and clones address the same agent.

### client.prompt()

```rust
pub async fn prompt(&self, options: AgentOptions, prompt: impl Into<Prompt>) -> Result<String>;
```

One-shot convenience: creates an agent, sends a single prompt, waits for the run to finish, and closes the agent — including when the run fails, so a failed turn doesn't leak it.

```rust
use cursor_sdk::{AgentOptions, Client};

let client = Client::new();
let answer = client
    .prompt(
        AgentOptions::local(".").model("composer-2.5"),
        "What does the auth middleware do?",
    )
    .await?;
println!("{answer}");
client.close().await?;
# Ok::<(), cursor_sdk::Error>(())
```

The bridge stays up for reuse; call `client.close()` when the program is done with it.

## Sending messages

Each `agent.send()` returns a `Run`. The agent retains conversation context across runs; the run is the unit of work for one prompt.

```rust
println!("{}", agent.ask("Find the bug in src/auth.rs").await?);

// Same agent, full conversation context is preserved.
println!("{}", agent.ask("Fix it and add a regression test").await?);
# Ok::<(), cursor_sdk::Error>(())
```

To send images alongside text:

```rust
use cursor_sdk::{Image, Prompt};

let run = agent
    .send(Prompt::text("What's in this screenshot?").with_image(Image::from_file("shot.png")?))
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

`Image::from_file` reads from disk and base64-encodes for you. `Image::bytes(data, mime_type)` and `Image::url(url)` are also available for callers that already have the bytes or a remote URL. Remote URLs are accepted only by cloud-routed agents in `sdk.v1`.

### Run

```rust
impl Run {
    pub fn run_id(&self) -> Option<&str>;
    pub fn agent_id(&self) -> &str;
    pub fn last_offset(&self) -> Option<&str>;
    pub fn outcome(&self) -> Option<&RunOutcome>;

    pub async fn next_event(&mut self) -> Option<Result<RunEvent>>;
    pub async fn next_text(&mut self) -> Option<Result<String>>;
    pub fn into_stream(self) -> impl Stream<Item = Result<RunEvent>>;

    pub async fn wait(self) -> Result<RunOutcome>;
    pub async fn text(self) -> Result<String>;
    pub async fn cancel(&self) -> Result<()>;
    pub async fn resume(&mut self) -> Result<()>;
}
```

`wait()` and `text()` consume the run, because there is nothing left to read afterwards. `cancel()` takes `&self`, so you can cancel and then keep consuming events to observe the terminal result.

A run stream is consumable once. `next_event()`, `next_text()`, and `into_stream()` all draw from the same underlying stream and advance it.

`run.run_id()` is `None` until the first event reveals it — the run typically opens with a `system` message of subtype `init` that carries it.

### Streaming

```rust
use cursor_sdk::RunEvent;

let mut run = agent.send("Find the bug in src/auth.rs").await?;

while let Some(event) = run.next_event().await {
    match event? {
        RunEvent::Message(message) => match message.kind.as_str() {
            "assistant" => {
                if let Some(text) = message.text() {
                    print!("{text}");
                }
            }
            "thinking" => eprintln!("[thinking]"),
            "tool_call" => eprintln!(
                "[tool] {}: {}",
                message.payload["name"], message.payload["status"]
            ),
            "status" => eprintln!("[status] {}", message.payload["status"]),
            _ => {}
        },
        RunEvent::Completed(outcome) => {
            println!("\n{} in {:?}", outcome.status, outcome.duration);
        }
        _ => {}
    }
}
# Ok::<(), cursor_sdk::Error>(())
```

Three things the crate handles so you don't have to:

- **Keepalives are invisible.** An idle stream gets an empty frame roughly every 15 seconds. It is consumed internally and never surfaces as an event or as end-of-stream.
- **Unknown events are skipped.** Envelope cases and message kinds newer than your crate version are dropped silently, so a bridge upgrade cannot break a running program.
- **Offsets are tracked.** The last durable offset is kept for [resume](#resuming-a-dropped-stream).

For `futures` combinators, `run.into_stream()` gives you a `Stream<Item = Result<RunEvent>>`.

### Reading text output

`next_text()` yields assistant text as it streams, skipping everything else. `text()` consumes the run and returns the final terminal text.

```rust
while let Some(chunk) = run.next_text().await {
    print!("{}", chunk?);
}
# Ok::<(), cursor_sdk::Error>(())
```

```rust
let final_text = agent.send("Explain this module").await?.text().await?;
# Ok::<(), cursor_sdk::Error>(())
```

`text()` returns an error when the run did not finish successfully, so a caller that only wants the answer never silently receives an empty string from a failed run. Use `wait()` when you want to inspect a failure yourself.

### Waiting without streaming

```rust
let outcome = agent.send("Refactor the parser").await?.wait().await?;

println!("{}", outcome.status);        // finished | error | cancelled | expired | ...
println!("{}", outcome.text);          // final assistant text
println!("{:?}", outcome.model);       // resolved ModelChoice used for this run
println!("{:?}", outcome.duration);
println!("{:?}", outcome.usage);       // cumulative TokenUsage, or None
println!("{:?}", outcome.git);         // branches and PR URLs on cloud
# Ok::<(), cursor_sdk::Error>(())
```

`RunOutcome` is the same shape whether it came from a terminal stream event, `wait()`, `client.get_run()`, or `agent.runs()`.

```rust
pub struct RunOutcome {
    pub run_id: String,
    pub agent_id: String,
    pub status: RunStatus,
    pub text: String,
    pub model: Option<ModelChoice>,
    pub duration: Duration,
    pub git: Vec<GitBranch>,
    pub created_at: Option<SystemTime>,
    pub usage: Option<TokenUsage>,
    pub error_code: Option<String>,
    pub failure_message: Option<String>,
}

pub struct GitBranch {
    pub repo_url: String,
    pub branch: String,
    pub pr_url: String,
}
```

`RunStatus` is `Creating`, `Running`, `Finished`, `Error`, `Cancelled`, `Expired`, or `Unknown`, with `is_terminal()` and `is_success()` helpers. `Unknown` covers a value newer than your crate version, so a match on it never panics.

### Run failures are not RPC failures

A run that fails still ends with a **successful** stream: you get a `RunOutcome` whose status is `Error`, `Cancelled`, or `Expired`. Reserve error handling for transport and request problems; read run outcomes from the outcome.

The readable reason is the subtle part. `error_code` is frequently empty even on a failed run — the human-readable text arrives in the `status` message's `message` field instead. The crate captures the last such message and `failure_reason()` prefers it:

```rust
let outcome = agent.send("Do the thing").await?.wait().await?;

if let Some(reason) = outcome.failure_reason() {
    eprintln!("run did not succeed: {reason}");
}
# Ok::<(), cursor_sdk::Error>(())
```

`failure_reason()` returns `None` for a successful run, the status message when there is one, then `error_code`, then a description of the status.

### Token usage

Runs report token usage when the runtime provides it. Read it from `outcome.usage`, which is summed across every turn that reported usage, and `None` when no turn did — a cancelled run that never finished a turn, or a runtime that doesn't surface usage.

```rust
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub reasoning_tokens: Option<i64>,
}
```

| Field | Description |
| --- | --- |
| `input_tokens` | Prompt tokens sent to the model. |
| `output_tokens` | Tokens generated by the model. |
| `cache_read_tokens` | Tokens served from the prompt cache. |
| `cache_write_tokens` | Tokens written to the prompt cache. |
| `total_tokens` | `input + output + cache_read + cache_write`. Excludes `reasoning_tokens`. |
| `reasoning_tokens` | Reasoning tokens, a subset of `output_tokens`. `None` when the model or runtime didn't report it. |

```rust
match outcome.usage {
    Some(usage) => println!(
        "total {}, in {}, out {}, cache r/w {}/{}",
        usage.total_tokens,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    ),
    None => println!("no usage reported for this run"),
}
```

`reasoning_tokens` is already counted inside `output_tokens`, so `total_tokens` leaves it out to avoid double-counting.

For per-turn numbers as they stream, handle the `usage` [stream message](#stream-events). It fires once at the end of each turn that reported usage.

```rust
while let Some(event) = run.next_event().await {
    if let RunEvent::Message(message) = event? {
        if message.kind == "usage" {
            println!("turn used {} tokens", message.payload["usage"]["totalTokens"]);
        }
    }
}
# Ok::<(), cursor_sdk::Error>(())
```

Token counts are what the runtime reports; they say nothing about cost. For billed usage and dollar cost, call [`agent.usage()`](#agentusage).

### Cancelling a run

```rust
run.cancel().await?;
# Ok::<(), cursor_sdk::Error>(())
```

Requests cancellation of an active run. The stream still delivers a terminal outcome with status `Cancelled`, so keep consuming events afterwards if you want it. In-flight tool calls stop, and partial assistant text written so far stays in the outcome.

Cancelling before the run id has arrived is a clear error rather than a silent no-op:

```text
invalid configuration: this run has no id yet, so it cannot be cancelled;
the id arrives with the first stream event
```

To cancel without a `Run` handle, use `client.cancel_run(run_id, Some(agent_id))` or `agent.cancel_run(run_id)`.

### Resuming a dropped stream

Dropping a `Send` stream does **not** cancel the run. It keeps executing on the bridge, and there are two ways back to it.

```rust
let mut run = agent.send("Long refactor").await?;

// ... the connection dies mid-stream ...

run.resume().await?;              // reconnect to the durable event log
let outcome = run.wait().await?;  // or just wait; it falls back to WaitLiveRun
# Ok::<(), cursor_sdk::Error>(())
```

`wait()` falls back to a blocking wait on the run when the stream fails before a terminal result, so the common case needs no special handling at all.

`resume()` enforces an offset rule that is easy to get wrong by hand. Offsets are scoped to the *stream kind* as well as the run: a live `Send` stream interleaves non-durable events into its numbering, so a live offset is not a valid durable resume point and passing one can silently skip events. So:

- A run that was streaming live replays the durable log **from the beginning**. De-duplicate on your side.
- A run already reading the durable log resumes **exactly after** its last offset.

To attach to a run you have only the ID for, use `client.observe_run(run_id, after_offset)` or `agent.observe(run_id, after_offset)`. Only pass an offset that a previous observe produced.

### Reading run state

```rust
println!("{:?}", run.run_id());
println!("{}", run.agent_id());
println!("{:?}", run.last_offset());
println!("{:?}", run.outcome());   // Some(..) once the terminal event arrived

let document = client.run_conversation("run_1").await?;  // serde_json::Value
# Ok::<(), cursor_sdk::Error>(())
```

`run_conversation()` returns the run's conversation document as parsed JSON. Use it to render or persist structured history without subscribing to the live stream. Unlike the TypeScript and Python SDKs, this crate does not model conversation turns as typed structs — the bridge hands over an opaque JSON document and the crate parses it without reshaping it.

### Per-run model override

The model you pass on send overrides the agent's selection for that run, then becomes sticky server-side: subsequent sends without an override continue to use the new model.

```rust
use cursor_sdk::{ModelChoice, SendOptions};

let run = agent
    .send_with(
        "Plan the refactor",
        SendOptions::new().model(ModelChoice::new("composer-2.5").with_param("fast", "true")),
    )
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

`outcome.model` reflects the selection this run actually used and is immutable once the run starts. Note that `agent.model()` is a local snapshot taken at creation; it does not update after a sticky override. Read `outcome.model` for the authoritative value.

### Per-run environment variables

Cloud agents can take environment variables for a single run. The values are injected into the agent's shell for that run only; when the run finishes they're removed from the VM and the next run doesn't see them. This is the right shape for credentials that rotate between turns.

```rust
use cursor_sdk::SendOptions;

let run = agent
    .send_with(
        "Deploy the preview environment",
        SendOptions::new().cloud_env_var("DEPLOY_TOKEN", mint_short_lived_token().await?),
    )
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

If a run-scoped variable has the same name as an agent-scoped one, the run-scoped value wins for that run, then the agent-scoped value comes back on the next run.

Per-run environment variables are cloud agents only, and aren't available for agents running against public repositories. For local agents, the bridge process inherits the environment you gave it — use `Client::builder().bridge_env(key, value)`.

### Conversation mode

Pass `AgentMode::Plan` or `AgentMode::Agent` to control whether a run explores and plans first or implements changes directly. See [Plan mode](/help/ai-features/plan-mode) for what plan mode does in the product.

Set it on `AgentOptions` to seed the first run. On follow-up sends, omit it to keep the conversation's current mode, or pass it to switch for that run only.

```rust
use cursor_sdk::{AgentMode, AgentOptions, CloudAgent, SendOptions};

let agent = client
    .create_agent(
        AgentOptions::new()
            .model("composer-2.5")
            .mode(AgentMode::Plan)
            .cloud_options(CloudAgent::new("https://github.com/your-org/your-repo")),
    )
    .await?;

agent.send("Design the auth refactor").await?.wait().await?;
agent
    .send_with("Looks good, start building", SendOptions::new().mode(AgentMode::Agent))
    .await?
    .wait()
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

### Streaming raw deltas

Events are whole messages by default. For lower-level updates — per-token text, tool-call args streaming in, thinking deltas, step boundaries — opt in with `deltas(true)` and `steps(true)`, and they arrive as their own event variants.

```rust
use cursor_sdk::{RunEvent, SendOptions};

let mut run = agent
    .send_with("Refactor the utils module", SendOptions::new().deltas(true).steps(true))
    .await?;

while let Some(event) = run.next_event().await {
    match event? {
        RunEvent::Delta { kind, payload } if kind == "text-delta" => {
            print!("{}", payload["text"].as_str().unwrap_or_default());
        }
        RunEvent::Step { kind, .. } => eprintln!("[step] {kind}"),
        _ => {}
    }
}
# Ok::<(), cursor_sdk::Error>(())
```

They are off by default because they multiply event volume considerably, and most callers want whole messages.

### SendOptions

| Method | Description |
| --- | --- |
| `model(impl Into<ModelChoice>)` | Per-send model override. Sticky after a successful send. |
| `mode(AgentMode)` | Per-send conversation mode override. |
| `mcp_server(name, McpServer)` | Inline MCP server definitions. Fully replaces creation-time servers for this run. |
| `deltas(bool)` | Emit raw `InteractionUpdate` deltas as `RunEvent::Delta`. Off by default. |
| `steps(bool)` | Emit completed conversation steps as `RunEvent::Step`. Off by default. |
| `cloud_env_var(key, value)` | Cloud agents only. [Per-run environment variables](#per-run-environment-variables). |
| `force(bool)` | Local agents only. Expire a stuck active run before starting this message. |

For an idempotency key, use `agent.send_idempotent(prompt, options, key)`.

---

The next two sections are detailed reference for stream events and payload shapes. Skim or skip on a first read; [Resuming agents](#resuming-agents) picks up the narrative.

## Stream events

`run.next_event()` yields `RunEvent`. Match on the variant, then on `message.kind` for conversation messages.

```rust
pub enum RunEvent {
    Message(StreamMessage),
    Delta { kind: String, payload: serde_json::Value },
    Step { kind: String, payload: serde_json::Value },
    Completed(Box<RunOutcome>),
}

pub struct StreamMessage {
    pub kind: String,
    pub payload: serde_json::Value,
}
```

`Completed` is boxed because it carries a whole `RunOutcome` and arrives once, while `Message` arrives constantly; boxing keeps the common event small.

A normal live stream is `Message*` then `Completed`, then the stream ends.

### Message kinds

| `kind` | Description | Key payload fields |
| --- | --- | --- |
| `system` | Init metadata. Emitted once at the start of a run. | `subtype` (`"init"`), `model`, `tools` |
| `user` | Echo of the user prompt for this run. | `message.content` |
| `assistant` | Model text output. | `message.content` with `text` and `tool_use` blocks |
| `thinking` | Reasoning content. | `text`, `thinking_duration_ms` |
| `tool_call` | Tool invocation lifecycle. Emitted at start with `args`, then again on completion with `result`. | `call_id`, `name`, `status`, `args`, `result`, `truncated` |
| `status` | Lifecycle transitions. On failure this carries the readable reason. | `status`, `message` |
| `task` | Task-level milestones and summaries. | `status`, `text` |
| `request` | Awaiting user input or approval. | `request_id` |
| `usage` | Per-turn token usage, emitted once at turn end. | `usage` |

Switch on `kind` and ignore values you don't know; new ones are added over time and the crate passes them through rather than dropping them.

> **Tool call schema is not stable.** The `args` and `result` payloads on `tool_call` events reflect each tool's internal shape and can change as tools evolve. Tool names can also be renamed or replaced. Treat them as untyped JSON and parse defensively. The envelope (`kind`, `call_id`, `name`, `status`) is stable.

### Reading payloads

Payloads are `serde_json::Value`, which keeps the crate honest about a wire format that is documented as free-form. Two helpers cover the common cases:

```rust
message.text()          // Option<String>: assistant text, whatever shape it arrived in
message.is_assistant()  // bool
message.run_id()        // Option<&str>
message.agent_id()      // Option<&str>
```

`text()` is tolerant of the several shapes the payload takes — a content-block array under `message.content` or `content`, or a plain `text` field — and concatenates the text blocks. `run_id()` and `agent_id()` find the ids under either snake_case or camelCase.

For anything else, index the JSON directly, or deserialize a slice of it into your own struct with `serde_json::from_value`.

## Resuming agents

```rust
pub async fn resume_agent(&self, agent_id: impl Into<String>, options: AgentOptions) -> Result<Agent>;
```

Use `client.resume_agent()` to reattach to an existing agent by ID, applying updated options. Common flows: reconnecting to a long-running cloud agent kicked off earlier, or continuing a conversation after the process restarted. Runtime is auto-detected from the ID prefix (`bc-` is cloud, anything else is local).

```rust
use cursor_sdk::{AgentOptions, Client};

let client = Client::new();
let agent = client
    .resume_agent("bc-abc123", AgentOptions::local(".").model("composer-2.5"))
    .await?;

println!("{}", agent.ask("Also update the changelog").await?);
# Ok::<(), cursor_sdk::Error>(())
```

When you only need a handle to issue operations and don't need to update options, `client.agent(agent_id)` builds one with no RPC at all. It has no model or `cwd` recorded, so calls that route by working directory need one supplied another way.

Inline MCP servers are not persisted across resume — they often carry secrets and live in memory only. Pass them again on resume, or use file-based MCP config for servers that should survive. The same applies to `tools`, `disallowed_tools`, and custom tool declarations.

### Local persistence

Local agents persist conversation state and run metadata through the bridge, so follow-ups and resume survive a process restart. The bridge keeps this under a per-workspace state root on disk by default. Cloud agents persist server-side, so resuming a cloud agent from anywhere returns the same conversation.

Local persistence is workspace-scoped. Give the bridge the same workspace as the agent so local list, get, and resume calls resolve the right agents:

```rust
use cursor_sdk::{Client, ListAgents, RuntimeFilter};

let client = Client::builder().workspace("/path/to/repo").build();

let agents = client
    .list_agents(ListAgents::new().runtime(RuntimeFilter::Local).cwd("/path/to/repo"))
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

To swap where that state lives, see [Custom agent stores](#custom-agent-stores).

## Inspecting agents and runs

List, fetch, and manage past agents. List endpoints return a `Page<T>` for cursor-based pagination.

```rust
use cursor_sdk::{Client, ListAgents, ListMessages, ListRuns, RuntimeFilter};

let client = Client::new();

let page = client
    .list_agents(ListAgents::new().runtime(RuntimeFilter::Local).cwd(".").limit(20))
    .await?;

for info in &page.items {
    println!("{} {} {}", info.id, info.status, info.name);
}

if let Some(cursor) = page.next_cursor {
    let next = client.list_agents(ListAgents::new().cursor(cursor)).await?;
    println!("{} more", next.items.len());
}

// Or walk every page.
let all = client.list_all_agents(ListAgents::new()).await?;

let info = client.get_agent(&all[0].id).await?;
let agent = client.agent(&info.id);
let runs = agent.runs(ListRuns::new().limit(10)).await?;
let messages = agent.messages(ListMessages::new().limit(50)).await?;
# Ok::<(), cursor_sdk::Error>(())
```

`Page<T>` exposes `items`, `next_cursor`, and `has_more()`, and iterates directly with `for item in page`. `list_all_agents()` follows cursors for you — convenient, but it fetches every page, so prefer an explicit limit when the account has many agents.

| Call | Purpose |
| --- | --- |
| `client.list_agents(ListAgents)` | One page of agents. |
| `client.list_all_agents(ListAgents)` | Every matching agent, following cursors. |
| `client.get_agent(id)` | Metadata for one agent. |
| `client.agent(id)` | A handle with no RPC. |
| `agent.runs(ListRuns)` | One page of that agent's runs. |
| `client.get_run(run_id)` | A point-in-time snapshot of a run. |
| `client.wait_live_run(run_id)` | Block until a run is terminal. |
| `client.cancel_run(run_id, agent_id)` | Cancel without a `Run` handle. |
| `agent.messages(ListMessages)` | Stored messages for the agent. |
| `client.run_conversation(run_id)` | The run's conversation document, as JSON. |

`ListAgents` filters on `runtime`, `cwd`, `pr_url`, `include_archived`, `limit`, `cursor`, and `api_key`. `ListRuns` takes `runtime`, `limit`, and `cursor`. `ListMessages` takes `limit` and `offset`.

`AgentMessage` is distinct from a streamed `StreamMessage`:

```rust
pub struct AgentMessage {
    pub kind: String,
    pub uuid: String,
    pub agent_id: String,
    pub payload: serde_json::Value,
}
```

### AgentInfo

The metadata shape returned by `list_agents()` and `get_agent()`.

```rust
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub status: AgentStatus,              // Running | Finished | Error | Unknown
    pub last_modified: Option<SystemTime>,
    pub created_at: Option<SystemTime>,
    pub archived: bool,
    pub runtime: AgentRuntime,
}

pub enum AgentRuntime {
    Local { cwd: String },
    Cloud {
        env: Option<CloudEnvironment>,
        repos: Vec<String>,
        metadata: BTreeMap<String, String>,
    },
    Unknown,
}
```

The runtime is an enum rather than a bag of optional fields, so cloud-only data like `repos` and `metadata` is only reachable once you've established the agent is a cloud agent.

### Cloud agent lifecycle

Cloud agents stay in your team's workspace until you archive or delete them. `list_agents()` hides archived agents by default; pass `include_archived(true)` to see them. Filter by `pr_url` to find the agent that opened a specific pull request.

```rust
agent.archive().await?;     // soft-delete; transcript stays readable
agent.unarchive().await?;   // restore an archived agent
agent.delete().await?;      // permanent; subsequent reads are NotFound
# Ok::<(), cursor_sdk::Error>(())
```

`agent.close()` is different from all three: it releases local resources and leaves durable state alone.

### agent.usage()

Fetch billed token usage and dollar cost for an agent's runs. Pass a run id to restrict the result to one entry.

```rust
let usage = agent.usage().await?;

println!("tokens: {}", usage.usage.total_tokens);
if let Some(cost) = usage.cost {
    println!("charged: ${:.2}", cost.charged_cents / 100.0);
}
for run in &usage.runs {
    println!("{} {}", run.run_id, run.usage.total_tokens);
}
# Ok::<(), cursor_sdk::Error>(())
```

```rust
pub struct AgentUsage {
    pub usage: TokenUsage,          // summed across `runs`
    pub cost: Option<UsageCost>,    // summed across `runs`
    pub runs: Vec<RunUsage>,
}

pub struct RunUsage {
    pub run_id: String,
    pub usage: TokenUsage,
    pub cost: Option<UsageCost>,
}

pub struct UsageCost {
    pub raw_cost_cents: f64,   // undiscounted model token cost; 0 for request-priced usage
    pub charged_cents: f64,    // amount charged, discounts and the Cursor Token Fee included
}
```

Cost includes discounts and can take a moment to settle after a run ends; `cost` is `None` until it does. `charged_cents` is `0.0` for plan-included, BYOK, and credit-grant usage.

This is a different view from [Token usage](#token-usage): `outcome.usage` is the live token count for one run, while `usage()` is the billed record across the agent's runs.

Usage is **cloud agents only**. A local agent fails with a cloud-only error. Gate on the bridge's `agent.usage` capability when you need to know in advance:

```rust
if client.version().await?.has_capability("agent.usage") {
    let usage = agent.usage().await?;
    println!("{}", usage.usage.total_tokens);
}
# Ok::<(), cursor_sdk::Error>(())
```

## The catalog

Account-level and catalog reads, straight off the `Client`.

```rust
use cursor_sdk::Client;

let client = Client::new();

let me = client.me().await?;
let models = client.models().await?;
let repositories = client.repositories().await?;
# Ok::<(), cursor_sdk::Error>(())
```

`client.me()` returns a `User` with `api_key_name`, `user_id`, `email`, `first_name`, `last_name`, and `created_at`.

Use `client.models()` to discover valid model ids and per-model parameters before creating an agent. Parameters are model-specific; common examples are reasoning effort and Cursor Router's `optimize_for` on `auto-smart`.

```rust
let models = client.models().await?;

for model in &models {
    println!("{} — {}", model.id, model.display_name);
    for parameter in &model.parameters {
        let values: Vec<&str> = parameter.values.iter().map(|v| v.value.as_str()).collect();
        println!("    {} = {}", parameter.id, values.join(" | "));
    }
}

// Preset variants already carry valid params.
let composer = models.iter().find(|m| m.id == "composer-2.5");
if let Some(model) = composer {
    let choice = model.choice();   // ModelChoice with no overrides
    println!("{choice}");
}
# Ok::<(), cursor_sdk::Error>(())
```

The catalog is account- and team-specific. Cursor Router only appears as `auto-smart` when Router is available for the API key's team.

`client.repositories()` returns the SCM repositories available for cloud agents on the calling account or team. Each item exposes a `url`; use these to populate `CloudAgent::repositories`.

Catalog calls hard-require an API key on the request — current bridges fail closed rather than falling back to the bridge's environment. The crate raises this before the call goes out if no key is configured.

### Best practices

- **Discover, don't hard-code.** Call `client.models()` at startup and cache the result. Model ids and parameter shapes change as new models ship.
- **Pass parameters explicitly when the model expects them.** A model whose `parameters` list is non-empty is a parameterized model. Otherwise the run uses each parameter's first allowed value, which may not be what you intend.
- **Resolve by capability, not id**, when you want "the current default in fast mode" rather than a specific model.

## The client and the bridge

`Client::new()` is the zero-configuration path. `Client::builder()` covers everything else.

```rust
use std::time::Duration;
use cursor_sdk::{Client, LocalStore};

let client = Client::builder()
    .api_key(std::env::var("CURSOR_API_KEY").unwrap())
    .workspace("/path/to/repo")
    .bridge_binary("/opt/cursor-sdk-bridge/bin/cursor-sdk-bridge")
    .request_timeout(Duration::from_secs(60))
    .startup_timeout(Duration::from_secs(30))
    .local_store(LocalStore::Jsonl { root_dir: "/var/lib/cursor-agents".into() })
    .verbose(true)
    .build();
```

| Method | Description |
| --- | --- |
| `api_key(key)` | The Cursor API key. Defaults to `CURSOR_API_KEY`. |
| `workspace(path)` | The bridge's workspace root: the default `cwd` for local agents and store discovery. |
| `bridge_binary(path)` | Use a specific executable instead of discovering one. |
| `endpoint(url, token)` | Attach to a bridge somebody else runs instead of spawning one. |
| `request_timeout(d)` | Deadline for a unary RPC, and for a stream's response headers. Never bounds how long a stream stays open. |
| `startup_timeout(d)` | How long to wait for the bridge's ready line. Defaults to 30s. |
| `shutdown_timeout(d)` | How long a graceful shutdown may take before the process is killed. Defaults to 5s. |
| `shutdown_grace(d)` | How long the bridge may drain in-flight RPCs during shutdown. Defaults to zero (exit immediately). |
| `local_store(LocalStore)` | Where the bridge keeps durable local agent state. |
| `agent_store(impl AgentStore)` | Own that state in your process. See [Custom agent stores](#custom-agent-stores). |
| `register_tool(CustomTool, handler)` | Register a [custom tool](#custom-tools). |
| `tool_registry(ToolRegistry)` | Share one registry across several clients. |
| `bridge_env(k, v)` / `bridge_arg(s)` | Extra environment variables and CLI flags for the bridge process. |
| `verbose(bool)` | Make the bridge log every RPC's name, outcome, duration, and error. Payloads are never logged. |
| `client_language(s)` | The language reported to Cursor for traffic attribution. Defaults to `rust`. |
| `verify_on_connect(bool)` | Ping and check the protocol version on first connect. On by default. |

### Bridge lifecycle

One managed bridge per client, created lazily on the first call that needs it.

```rust
let client = Client::new();

println!("{}", client.ping().await?);            // "pong"
let version = client.version().await?;
println!("{} {}", version.bridge_version, version.protocol_version);
println!("{:?}", version.capabilities);

if let Some(info) = client.bridge_info().await? {
    println!("{} pid {:?}", info.url, info.pid);
}

client.close().await?;
# Ok::<(), cursor_sdk::Error>(())
```

`client.close()` shuts the bridge down gracefully: `Shutdown`, then wait, then kill if it overstays. Dropping the last `Client` clone without closing — including on a panic unwind — still kills the process, so a bridge cannot outlive the program on any normal path.

Signals are the exception, since they tear the process down without running destructors. Binaries that care can close that gap:

```rust
cursor_sdk::install_exit_guard();
```

This installs a process-wide handler for Ctrl-C and `SIGTERM` that kills managed bridges. It is opt-in because a library should not hijack a program's signals without being asked.

### Attaching to an existing bridge

If your platform already runs `cursor-sdk-bridge`, attach to it rather than launching another:

```rust
let client = Client::builder().endpoint("http://127.0.0.1:43210", bridge_token).build();
```

An attached client never stops that process, so `close()` on it is a no-op for the bridge. A [custom agent store](#custom-agent-stores) cannot be used with an attached bridge — the bridge can only be told about one at launch — and the crate says so rather than failing obscurely later.

### Capability negotiation

`client.version()` reports the bridge build version, the protocol version, and a list of capability strings. Gate optional features on capabilities rather than on a version comparison; unknown strings are forward-compatible additions.

```rust
let version = client.version().await?;
assert!(version.speaks_supported_protocol());   // protocol_version == "sdk.v1"
if version.has_capability("artifacts.chunked") {
    // ...
}
# Ok::<(), cursor_sdk::Error>(())
```

A bridge reporting a protocol version this crate wasn't generated against is logged as a warning, not treated as fatal: `sdk.v1` only changes additively, so a mismatch is far more likely a newer bridge than a broken one.

### Calling an RPC the crate hasn't wrapped

When a newer bridge adds an RPC before this crate wraps it, you don't have to wait for a release:

```rust
use cursor_sdk::proto;

let response: proto::PingResponse = client
    .call_unary("SdkBridgeControlService", "Ping", &proto::PingRequest {})
    .await?;
# Ok::<(), cursor_sdk::Error>(())
```

`call_server_stream` is the streaming counterpart. These are the only places raw protobuf types appear in the public API; everything else is plain Rust.

## MCP servers

Agents can pick up MCP servers from inline definitions, project and user settings, plugins, and dashboard-managed configuration depending on the runtime.

```rust
use cursor_sdk::{AgentOptions, McpAuth, McpServer, McpTransport};

let options = AgentOptions::local(".")
    .model("composer-2.5")
    .mcp_server(
        "docs",
        McpServer::Http {
            transport: McpTransport::Http,
            url: "https://example.com/mcp".into(),
            headers: Default::default(),
            auth: Some(McpAuth {
                client_id: "client-id".into(),
                client_secret: String::new(),
                scopes: vec!["read".into(), "write".into()],
            }),
        },
    )
    .mcp_server(
        "filesystem",
        McpServer::stdio("npx", ["-y", "@modelcontextprotocol/server-filesystem", "."]),
    );
```

`McpServer::stdio(command, args)` and `McpServer::http(url)` are shorthands for the common cases.

### What gets loaded

**Local agents** load servers from up to five sources, with first-match-wins precedence on conflicting names:

1. `mcp_server` on send. Fully replaces creation-time servers for that run (not merged).
2. `mcp_server` on `AgentOptions`. Used when no per-send override is provided.
3. Plugin servers, if `setting_sources` includes `Plugins`.
4. Project servers from `.cursor/mcp.json`, if `setting_sources` includes `Project`.
5. User servers from `~/.cursor/mcp.json`, if `setting_sources` includes `User`.

Without `setting_sources`, only inline servers are loaded. If a local MCP server requires OAuth login, the SDK can reuse a saved login from the Cursor app, but it cannot open a browser to sign you in.

**Cloud agents** load servers from:

1. `mcp_server` on send.
2. `mcp_server` on `AgentOptions`.
3. Your user and team MCP servers from [cursor.com/agents](https://cursor.com/agents).

If an inline server doesn't include `auth` or `headers` and you've previously authorized that server URL on cursor.com/agents, runs authenticated with a personal API token reuse those OAuth tokens automatically. Service account API keys cannot fall back to user auth.

`setting_sources` does not apply to cloud agents.

### Credentials

- HTTP `headers` and `auth` are handled by Cursor's backend. Sensitive fields are redacted and do not enter the VM.
- Stdio `env` values are passed into the VM because the server runs there. Treat them like any other runtime secret.
- OAuth for MCP servers configured on cursor.com/agents stays per-user, even for team-level servers.

See [MCP](/docs/mcp) for the full config format.

## Subagents

Define named subagents that the main agent can spawn via the `Agent` tool.

```rust
use cursor_sdk::{AgentOptions, SubAgent};

let options = AgentOptions::local(".")
    .model("composer-2.5")
    .sub_agent(
        "code-reviewer",
        SubAgent {
            inherit_model: Some(true),
            ..SubAgent::new(
                "Expert code reviewer for quality and security.",
                "Review code for bugs, security issues, and proven approaches.",
            )
        },
    )
    .sub_agent(
        "test-writer",
        SubAgent::new(
            "Writes tests for code changes.",
            "Write comprehensive tests for the given code.",
        ),
    );
```

Subagents committed to the repo at `.cursor/agents/*.md` (with `name`, `description`, and optional `model` frontmatter) are also picked up. Inline definitions override file-based ones with the same name.

`SubAgent` also takes `model` for an explicit override and `mcp_servers` for the servers it may reach, either by name from the parent (`SubAgentMcpServer::Named`) or inline (`SubAgentMcpServer::Inline`).

### Nested subagents

Subagents can spawn their own subagents, within a nesting limit. Each level reaches the same set of named subagents and [custom tools](#custom-tools). The top-level agent and its direct subagents can launch subagents, but a subagent launched by another subagent can't launch further ones.

### Background subagents

When the agent runs a subagent in the background, the subagent's result returns to the parent as a follow-up turn on the same run instead of being dropped when the parent turn ends. The event stream keeps yielding through those turns, and `wait()` returns after them. Local agents only.

## Restricting the toolset

`tools` allowlists the built-in tools offered to the model; `disallowed_tools` removes tools and keeps the rest, including tools added to the platform after your crate version was released.

```rust
use cursor_sdk::AgentOptions;

// Read-only agent: only these tools are offered.
let reader = AgentOptions::local(".")
    .model("composer-2.5")
    .tools(["read", "grep", "glob", "ls"]);

// Everything except shell access.
let no_shell = AgentOptions::local(".")
    .model("composer-2.5")
    .disallowed_tools(["shell"]);
```

- Omitting `tools` offers the standard toolset for the selected model. `tools([])` offers **no** built-in tools, so the model can only respond with text.
- That distinction is preserved on the wire: "unset" and "empty" are genuinely different, not collapsed into one.
- Both accept public names (`"read"`, `"edit"`, `"task"`, `"webSearch"`, …), the capability groups `"shell"` and `"mcp"`, and raw proto tool names. Unknown names fail agent creation.
- Deny wins: a tool must be in `tools` (when set) and not in `disallowed_tools` to be offered.
- Disallowing `"mcp"` also removes [custom tools](#custom-tools). Disallowing `"task"` prevents [subagents](#subagents).

Both are local agents only for now, and neither persists on the agent: pass them again on resume to keep the restriction.

## Custom tools

Custom tools let you expose Rust functions to local agents without standing up a separate MCP server. The SDK runs a loopback Connect server; the bridge authenticates to it with a bearer token this process chose, and calls back into your code when the agent invokes a tool.

Declaring a tool and implementing it are one step:

```rust
use cursor_sdk::{Client, CustomTool};
use serde_json::json;

let client = Client::builder()
    .register_tool(
        CustomTool::new(
            "deployment_status",
            "Look up the current deployment status for a service.",
            json!({
                "type": "object",
                "properties": {
                    "service": {"type": "string", "description": "Service name"},
                },
                "required": ["service"],
            }),
        ),
        |call| async move {
            // Anything async goes here: a database, an HTTP call, a queue.
            let service = call.require("service")?.as_str().unwrap_or_default().to_string();
            Ok(json!({"service": service, "version": "2026.9.3", "healthy": true}))
        },
    )
    .build();
```

Declarations registered on the client are merged into every local agent that client creates, so the tool the model can see is always one the client can actually run. An agent-level declaration of the same name wins, which is how you vary the description per agent.

```rust
let agent = client
    .create_agent(cursor_sdk::AgentOptions::local(".").model("composer-2.5"))
    .await?;

println!("{}", agent.ask("Is the checkout service deployed yet?").await?);
# Ok::<(), cursor_sdk::Error>(())
```

### The handler

```rust
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
    pub tool_call_id: Option<String>,
    pub agent_id: String,
}
```

| Helper | Description |
| --- | --- |
| `call.arg(name)` | The named argument, or `None`. |
| `call.string_arg(name)` | The named argument as `&str`. |
| `call.require(name)` | The argument, or an error naming the tool and the missing argument. |

The handler returns `Result<serde_json::Value, Box<dyn Error + Send + Sync>>`, so `?` works on anything that implements `std::error::Error`. An error is reported back to the agent as a tool failure, and the agent can try another approach.

A scalar return value is wrapped as `{"value": …}` on the way out, because a tool result is a protobuf `Struct` and can only encode an object. Returning a bare string therefore just works.

### Registering later

`client.register_tool(...)` adds a tool after the client is built, re-pointing a running bridge at the callback server if needed. Note that a tool only reaches a model if its declaration went out with the agent's options — so register before creating the agent that should use it.

```rust
client.tools().names();          // what is registered
client.tools().unregister("x");  // remove one
# Ok::<(), cursor_sdk::Error>(())
```

Custom tools are a **local agent** feature. Deny rules and [sandbox](#sandbox-options) limits still apply, but custom tools skip interactive approval, so sandboxed and [auto-review](#auto-review) runs call them without prompting. They reach [subagents](#subagents), including nested ones.

## Custom agent stores

By default the bridge persists local agent state itself — SQLite, or JSONL under a directory you pick:

```rust
use cursor_sdk::{Client, LocalStore};

let client = Client::builder()
    .local_store(LocalStore::Jsonl { root_dir: "/var/lib/cursor-agents".into() })
    .build();
```

To own that state yourself — a shared Postgres, Redis, an in-memory map for tests — implement `AgentStore`. The bridge then forwards every store operation to your code over a single callback RPC.

```rust
use cursor_sdk::{AgentStore, Client, HandlerResult, LoggingStore, MemoryStore, StoreRequest};
use cursor_sdk::callback::BoxFuture;
use serde_json::Value;

struct MyStore { /* a pool, a client, whatever */ }

impl AgentStore for MyStore {
    fn call<'a>(&'a self, request: StoreRequest) -> BoxFuture<'a, HandlerResult<Option<Value>>> {
        Box::pin(async move {
            match (request.substore.as_str(), request.method.as_str()) {
                ("agents", "get") => { /* ... */ Ok(None) }
                ("agents", "create") => Ok(Some(request.record().clone())),
                _ => Ok(None),
            }
        })
    }
}

let client = Client::builder().agent_store(MyStore { }).build();
```

A store can only be configured **before** the bridge launches, because agents may load state before any RPC arrives. That is why it lives on the builder and not on the client, and why it cannot be combined with an attached bridge.

### The three rules

Everything else is detail; these three decide whether a store works.

1. **Return the bare record.** `create` and `update` *inputs* wrap the record under a singular key (`{"agent": {…}}`), but the *output* must be the record itself. Echoing the wrapper back causes opaque internal errors in the bridge. `request.record()` unwraps it for you.
2. **`Ok(None)` means null.** A `get` miss and a `delete` return no output.
3. **Checkpoint blobs are base64 strings**, not byte arrays. `create`/`update` input is `{"agentId", "blobId", "data"}`; `get` returns `{"found": bool, "data": <base64>}`.

### Substores and methods

| Substore | What it holds |
| --- | --- |
| `agents` | One record per agent. |
| `runs` | One record per run. |
| `runEvents` | The append-only run event log. `append` input is `{"runId", "eventType", "payload"}`. |
| `checkpoints` | Content-addressed conversation blobs. |

Methods are `get`, `create`, `update`, `delete`, `list`, and `append` (`runEvents` only). `Substore` and `StoreMethod` both carry an `Other(String)` variant, so a substore or method newer than your crate stays readable instead of panicking.

### Building one

The crate ships two implementations to build on:

| Type | Use |
| --- | --- |
| `MemoryStore` | A working in-memory store. Good for tests and short-lived programs. |
| `LoggingStore<S>` | Wraps another store and traces every operation on the `cursor_sdk::store` target. |

The recommended workflow is to wrap `MemoryStore` in `LoggingStore`, run one real turn, and read the shapes off your own bridge before writing the real thing. One agent creation plus one send exercises most substores and methods:

```bash
RUST_LOG=cursor_sdk::store=debug cargo run --example custom_store
```

## Hooks

Hooks are file-based only. There is no programmatic hook callback. Hooks are a project policy boundary, not a per-run knob.

- **Local:** add `.cursor/hooks.json` to the repo passed as the agent's `cwd`, or `~/.cursor/hooks.json` for user-level hooks.
- **Cloud:** commit `.cursor/hooks.json` and its scripts to the repo. SDK-created cloud agents load project hooks automatically. On Enterprise plans, they also run team hooks and enterprise-managed hooks.

See [Hooks](/docs/hooks) for the configuration format.

## Sandbox options

Local agents run unsandboxed by default. The agent can read and write the working directory, execute shell commands, and reach the network without restriction — there is no human-in-the-loop approval flow in a headless run, so a sandbox-by-default would block legitimate tool calls silently.

```rust
use cursor_sdk::{AgentOptions, LocalAgent};

let options = AgentOptions::new()
    .model("composer-2.5")
    .local_options(LocalAgent::new(".").sandbox(true));
```

When you enable the sandbox, every shell tool call and shell-spawned process is constrained:

- **Filesystem.** Writes are limited to the working directory, temp directories, and paths you allow in `sandbox.json`. Reads are not restricted to the workspace.
- **Shell.** Commands run inside a platform sandbox (`bubblewrap` on Linux, `seatbelt` on macOS). Privileged operations are denied.
- **Network.** Outbound network is denied by default. Drop a `.cursor/sandbox.json` in the workspace listing allowed hosts to open specific ones.

If sandboxing isn't supported on the host, agent creation fails with a message naming the missing dependency. Cloud runs always execute inside an isolated VM, so this doesn't apply to them.

## Auto-review

Set `auto_review(true)` to route local tool calls through [Auto-review](/docs/agent/security/run-modes), the same classifier the IDE uses to allow or block Shell, MCP, and Fetch calls based on safety and intent.

```rust
use cursor_sdk::{AgentOptions, LocalAgent};

let options = AgentOptions::new()
    .model("composer-2.5")
    .local_options(LocalAgent::new(".").auto_review(true));
```

Because there's no interactive approval in a headless run, a call the classifier blocks is denied rather than escalated, and the agent gets the block reason and can try another approach. Steer it with a `permissions.json` `autoRun` block in the workspace.

Auto-review is local agents only, and it is best-effort convenience rather than a security boundary — combine it with [`sandbox`](#sandbox-options) for strict control.

## Artifacts

List and download files from the agent's workspace.

```rust
pub struct Artifact {
    pub path: String,
    pub size_bytes: u64,
    pub updated_at: String,
}
```

```rust
for artifact in agent.artifacts().await? {
    println!("{} ({} bytes)", artifact.path, artifact.size_bytes);
}

// Into memory.
let bytes = agent.download_artifact("out/report.md").await?;

// Or straight to disk, without holding the whole file.
let written = agent.download_artifact_to("out/report.md", "review.md").await?;
# Ok::<(), cursor_sdk::Error>(())
```

Artifacts arrive in chunks over a stream. `download_artifact_to` writes them through and creates parent directories as needed, which is the right call for anything large.

Artifact support is runtime-dependent. Local agents return an empty list and fail on download.

## Resource management

There are two things to release: the agent, and the bridge.

```rust
use cursor_sdk::{AgentOptions, Client};

let client = Client::new();
let agent = client.create_agent(AgentOptions::local(".").model("composer-2.5")).await?;

println!("{}", agent.ask("Summarize the repository").await?);

agent.close().await?;    // release the agent's local resources
client.close().await?;   // stop the bridge
# Ok::<(), cursor_sdk::Error>(())
```

`close()` is async, so it cannot run in `Drop`. For the common "close when this function returns, including on the error path" case, take a guard:

```rust
let agent = client.create_agent(options).await?;
let _guard = agent.close_on_drop();

// ... anything from here on, including `?`, still closes the agent.
# Ok::<(), cursor_sdk::Error>(())
```

`AgentGuard` spawns the close on the current runtime when it drops. Call `guard.close().await` instead when you want to see the failure.

The bridge needs no guard. Dropping the last `Client` clone kills it, including on a panic unwind, so it cannot outlive the program on any normal path. `client.close()` is the graceful version — `Shutdown`, wait, then kill — and the one that reports failures. For signals, see [`install_exit_guard`](#bridge-lifecycle).

## Configuration reference

### AgentOptions

| Method | Description |
| --- | --- |
| `AgentOptions::local(cwd)` | A local agent rooted at `cwd`. |
| `AgentOptions::cloud(repo)` | A cloud agent working on one repository. |
| `local_options(LocalAgent)` / `cloud_options(CloudAgent)` | A fully configured runtime. |
| `model(impl Into<ModelChoice>)` | The model. Required for local agents; cloud falls back to the server default. |
| `api_key(key)` | Override the client's key for this agent. |
| `name(name)` | Human-readable agent name surfaced in listings. |
| `agent_id(id)` | Durable agent ID. Pass to keep a stable ID across invocations. |
| `mode(AgentMode)` | Initial conversation mode. `Agent` or `Plan`. |
| `mcp_server(name, McpServer)` | Inline MCP server definitions. |
| `sub_agent(name, SubAgent)` | Subagent definitions. |
| `tools(names)` / `disallowed_tools(names)` | [Restrict the toolset](#restricting-the-toolset). Local agents only. |

### LocalAgent

| Method | Description |
| --- | --- |
| `LocalAgent::new(cwd)` | Primary working directory for the default shell and agent-store scoping. |
| `dirs(paths)` | Additional workspace folders for multi-root setups. Rules, skills, and workspace context load from every path. |
| `setting_sources(sources)` | Ambient settings layers to load. |
| `sandbox(bool)` | [Sandbox](#sandbox-options) shell tool calls. Default off. |
| `auto_review(bool)` | Route local tool calls through [Auto-review](#auto-review). Default off. |
| `store(LocalStore)` | Where the bridge keeps this agent's durable state. |
| `custom_tool(CustomTool)` | Declare a [custom tool](#custom-tools) for this agent specifically. |

### CloudAgent

| Method | Description |
| --- | --- |
| `CloudAgent::new(repo)` | One repository. |
| `CloudAgent::no_repository()` | An empty workspace. See [no-repo agents](#no-repo-cloud-agents). |
| `repositories(repos)` | Several repositories, up to 20. |
| `environment(kind, name)` | Execution environment: `Cloud`, `Pool`, or `Machine`. `Pool` and `Machine` target self-hosted workers you run. |
| `work_on_current_branch(bool)` | Push commits to the existing branch instead of a new one. |
| `auto_create_pr(bool)` | Open a PR when the run finishes. |
| `skip_reviewer_request(bool)` | Skip requesting the calling user as a reviewer. |
| `open_as_cursor_github_app(bool)` | Open PRs as the Cursor GitHub App. Defaults to `true` for service-account keys, `false` for user keys. |
| `env_var(k, v)` | [Session environment variables](#session-environment-variables). |
| `metadata(k, v)` | [Caller-owned tags](#agent-metadata). |

`CloudRepository::new(url).starting_ref("main")` sets the ref to start from; `pr_url` attaches the agent to an existing PR.

### ModelChoice

```rust
pub struct ModelChoice {
    pub id: String,
    pub params: BTreeMap<String, String>,
}
```

`ModelChoice::new("composer-2.5").with_param("fast", "true")`, or just `"composer-2.5"` anywhere a model is accepted. `id` is the model identifier; `params` carries per-model parameters such as reasoning effort or Router's `optimize_for`.

### McpServer

```rust
pub enum McpServer {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,       // local only; cloud rejects this field
    },
    Http {
        transport: McpTransport,    // Http | Sse
        url: String,
        headers: BTreeMap<String, String>,
        auth: Option<McpAuth>,      // { client_id, client_secret, scopes }
    },
}
```

### Prompt and Image

```rust
pub struct Prompt {
    pub text: String,
    pub images: Vec<Image>,
}

pub enum Image {
    Url { url: String, dimension: Option<(u32, u32)> },
    Data { base64: String, mime_type: String, dimension: Option<(u32, u32)> },
}
```

A `&str` or `String` converts into a `Prompt`, so `agent.send("hello")` works. `Image::from_file(path)`, `Image::bytes(data, mime_type)`, and `Image::url(url)` build the variants; `with_dimension(w, h)` records pixel dimensions.

### SettingSource

Controls which on-disk settings layers a local agent loads. Cloud agents always load `Project`, `Team`, and `Plugins` and ignore this field.

| Value | Source |
| --- | --- |
| `Project` | `.cursor/` in the workspace |
| `User` | `~/.cursor/` |
| `Team` | Team settings synced from the dashboard |
| `Mdm` | MDM-managed enterprise settings |
| `Plugins` | Plugin-provided settings |
| `All` | Shorthand for all of the above |

### BridgeManifest

What a standalone bridge archive says about itself. Every field is optional, so
older and newer manifests both parse.

```rust
pub struct BridgeManifest {
    pub bridge_version: Option<String>,
    pub sdk_version: Option<String>,
    pub os: Option<String>,        // "linux" | "darwin" | "win32"
    pub arch: Option<String>,      // "x64" | "arm64"
    pub protocol: Option<String>,  // "sdk.v1"
    pub entrypoint: Option<String>,
    pub distribution: Option<String>,
    pub runtime: Option<String>,
}
```

`BridgeManifest::for_binary(path)` reads the manifest beside a binary, returning
`None` when there isn't one. `speaks_supported_protocol()` compares against the
contract this crate was generated from. `manifest::host_os()` and
`manifest::host_arch()` give this host's tokens in the release vocabulary, which
is what you want when building an archive name. See
[the preflight](#the-manifest-preflight).

### Page

```rust
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
```

Returned by `list_agents()` and `agent.runs()`. `next_cursor` is `None` when there are no more pages; `has_more()` says the same thing. `Page<T>` iterates directly with `for item in page`.

## Errors

Every failure is an `Error`. Failed RPCs carry the bridge's structured error detail, mapped onto a stable `ErrorKind` you match on rather than parse.

```rust
pub enum Error {
    Bridge(BridgeError),      // finding, spawning, handshaking with, or stopping the bridge
    Transport(String),        // the HTTP connection failed, or a stream ended early
    Rpc(Box<RpcError>),       // the bridge answered an RPC with an error
    Decode { message, source },
    Io(std::io::Error),
    Config(String),           // caught before anything reached the wire
    Timeout { operation, timeout },
}
```

| Method | Description |
| --- | --- |
| `error.kind()` | `Option<ErrorKind>` — the stable class, for a failed RPC. |
| `error.request_id()` | The full Cursor Cloud request ID, when the bridge reported one. |
| `error.retry_after()` | The backend's suggested wait before retrying. |
| `error.is_retryable()` | Whether a retry could plausibly succeed without changing anything. |
| `error.is_auth()` / `error.is_not_found()` | Shorthands for the two most common checks. |

### ErrorKind

Derived from the bridge's `sdk_error_code` when one is attached, and from the Connect code otherwise.

| Kind | Meaning | Recommended fix |
| --- | --- | --- |
| `Unauthenticated` | Credentials missing or rejected. | Generate a new key from [API Keys](https://cursor.com/dashboard/api). Confirm the key has permission for the operation. |
| `PermissionDenied` | Plan or role forbids the operation; repository inaccessible. | Check the plan, the role, and SCM connection. |
| `NotFound` | Unknown agent or run. | Check the ID, the `cwd`, and the store. |
| `Validation` | Bad request, model, branch name, or PR URL. | Call `client.models()` to confirm the id and params. Check repo and file paths exist. |
| `RateLimited` | Rate or monthly usage limit hit. | Back off; honor `retry_after`. For a monthly cap, raise the plan's usage limit. |
| `AgentBusy` | The agent already has a run executing. | Wait for it, cancel it, or list runs before sending again. |
| `InvalidState` | Agent archived, or run already terminal. | Unarchive, or check `status` before cancelling. |
| `Upstream` | An upstream model provider failed. | Retry with backoff; `provider` names it. |
| `Internal` | Unexpected bridge or backend failure. | Retry with backoff. Log the `request_id`. |
| `Cancelled` | The client cancelled the operation. | — |
| `Unknown` | Unclassified, including codes newer than this crate. | Inspect `sdk_error_code` and the message. |

`Unknown` is the forward-compatibility escape hatch: a code this crate doesn't recognize still arrives with its raw integer intact on `RpcError::sdk_error_code`, so it stays diagnosable.

### RpcError

```rust
pub struct RpcError {
    pub kind: ErrorKind,
    pub connect_code: String,      // "unauthenticated", "not_found", ...
    pub sdk_error_code: i32,       // raw, so newer codes stay readable
    pub message: String,
    pub request_id: Option<String>,
    pub help_url: Option<String>,
    pub provider: Option<String>,
    pub retry_after: Option<Duration>,
    pub rate_limit: Option<RateLimit>,
    pub rpc: String,               // "SdkAgentService/Send"
}
```

Always log `request_id` in full — Cursor support traces requests by it, and a truncated ID is not traceable. `help_url` carries a one-click resolution link when the backend supplies one; the most common case is an SCM provider that isn't connected.

### Retrying with backoff

```rust
use cursor_sdk::{AgentOptions, Client, ErrorKind};
use std::time::Duration;

let client = Client::new();

let mut delay = Duration::from_secs(1);
for attempt in 0..3 {
    match client
        .prompt(AgentOptions::local(".").model("composer-2.5"), "Audit the auth middleware")
        .await
    {
        Ok(answer) => {
            println!("{answer}");
            break;
        }
        Err(error) if error.is_retryable() && attempt < 2 => {
            let wait = error.retry_after().unwrap_or(delay);
            eprintln!("{error} — retrying in {wait:?}");
            tokio::time::sleep(wait).await;
            delay *= 2;
        }
        Err(error) => {
            // Log the full request id so support has a handle on the failure.
            eprintln!("{error} (request {:?})", error.request_id());
            return Err(error);
        }
    }
}
# Ok::<(), cursor_sdk::Error>(())
```

`AgentBusy` is deliberately **not** retryable: retrying immediately will keep failing until the active run reaches a terminal status or you cancel it.

```rust
use cursor_sdk::{ErrorKind, ListRuns, RunStatus};

if let Err(error) = agent.send("Also add tests").await {
    if error.kind() == Some(ErrorKind::AgentBusy) {
        let runs = agent.runs(ListRuns::new().limit(1)).await?;
        if let Some(active) = runs.items.first() {
            if active.status == RunStatus::Running {
                agent.cancel_run(&active.run_id).await?;
            }
        }
        agent.send("Also add tests").await?;
    }
}
# Ok::<(), cursor_sdk::Error>(())
```

Local agents don't report `AgentBusy`. Use `SendOptions::new().force(true)` to expire a stuck local run before starting a new one.

### BridgeError

Process-level failures never carry a structured detail. They are adapter-side launch problems, and the bridge's captured stderr is usually the explanation.

| Variant | When |
| --- | --- |
| `NotFound` | No executable found, or an override points at a file that does not exist. The message lists every way to supply one. |
| `Spawn` | The process could not be started. |
| `ExitedBeforeReady` | The bridge exited before printing its ready line. Carries the captured stderr and the exit status. |
| `StartupTimeout` | The ready line did not arrive in time. |
| `Handshake` | The ready line was present but unusable — a schema, transport, or protocol this crate does not speak. |
| `AuthToken` | The bearer token could not be read from its file. |
| `Manifest` | The archive's `manifest.json` rules this binary out — wrong platform, or a contract this crate does not speak. See [the preflight](#the-manifest-preflight). |

The raw discovery line is never included in an error or a log, because older bridges inline the bearer token in it.

## Tracing and troubleshooting

Diagnostics go through [`tracing`](https://crates.io/crates/tracing). Four targets:

| Target | Carries |
| --- | --- |
| `cursor_sdk` | Connection and client-level events. |
| `cursor_sdk::bridge` | The bridge process's own stdout and stderr. |
| `cursor_sdk::callback` | Custom tool callbacks. |
| `cursor_sdk::store` | Custom store traffic, request and response. |

```bash
RUST_LOG=cursor_sdk=debug cargo run
```

Turn on the bridge's own RPC logging with `Client::builder().verbose(true)`, which logs each RPC's name, outcome, duration, and full error to the bridge's stderr and forwards it to the `cursor_sdk::bridge` target. Request and response payloads are never logged, and neither is the bearer token.

When an RPC fails and you suspect your own setup rather than the crate, the bridge repository ships a [curl-only smoke test](https://github.com/cursor/sdk-bridge/blob/main/docs/smoke-test.md) that exercises spawn, `Ping`, `Me`, `CreateAgent`, and `Send` with no adapter code. It answers "is it me or the bridge?" in one run.

## Known limitations

Most of these are contract limits rather than crate limits: the `sdk.v1` wire protocol has no field for them, so no adapter built on the bridge can offer them.

**Not in the `sdk.v1` contract:**

- **No `system_prompt`.** Replacing the built-in system prompt is a TypeScript-SDK option with no wire representation.
- **No steering.** `run.steer(text)` injects a message into a turn already running; there is no bridge RPC for it. Wait for the run and send a follow-up instead.
- **No interactive login.** There is no equivalent of `Cursor.auth.login()`. Set `CURSOR_API_KEY` or pass `api_key`.
- **No `request_id` on a run.** Runs do not carry a correlation ID on the wire. Errors do — see [`RpcError`](#rpcerror).
- **No workspace prewarming.** The first local send pays the workspace-resolution cost.

**Crate choices:**

- **Async only.** Every I/O method needs a Tokio runtime; there is no sync facade.
- **`conversation()` returns JSON**, not typed turns. The bridge hands over an opaque document and the crate parses it without reshaping it.
- **No status-change listener** and no `supports()` capability probe. Read the outcome, and gate on [`version().has_capability()`](#capability-negotiation).

**Same as the first-party SDKs:**

- Custom tools, custom stores, toolset restrictions, sandboxing, and auto-review are **local agents only**.
- `tools`, `disallowed_tools`, and custom tool declarations are **not persisted** on the agent. Pass them again on resume.
- Inline MCP servers are **not persisted** across resume; they often carry secrets. Pass them again, or use file-based MCP config.
- Artifacts and billed usage are **cloud agents only**. Local agents return an empty artifact list and fail on download and usage.
- `setting_sources` (and the file-based MCP and subagent paths it gates) does not apply to cloud agents. Cloud always loads `project`, `team`, and `plugins`.
- Hooks are file-based only (`.cursor/hooks.json`). No programmatic callbacks.
- Tool-call payload schemas are intentionally not strongly typed.

**Operational:**

- You need the `cursor-sdk-bridge` executable on the machine — `cargo install cursor-sdk-bridge-fetch && cursor-sdk-bridge-fetch`, or any of the other routes in [Installation](#installation). The library never downloads it for you, by design.
- A [custom agent store](#custom-agent-stores) cannot be used with an attached bridge, because the bridge can only be told about one at launch.
