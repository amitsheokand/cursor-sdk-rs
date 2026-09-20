//! Compile-checks for every API the Rust SDK documentation claims exists.
//!
//! Nothing here runs: the functions are never called. The point is that the
//! documented signatures must type-check, so a doc example cannot drift away
//! from the crate without `cargo test` noticing.

#![allow(dead_code, unused_variables, unreachable_code)]

use std::time::Duration;

use cursor_sdk::callback::BoxFuture;
use cursor_sdk::{
    AgentMode, AgentOptions, AgentRuntime, AgentStatus, AgentStore, Client, CloudAgent,
    CloudEnvironmentKind, CloudRepository, CustomTool, Error, ErrorKind, HandlerResult, Image,
    ListAgents, ListMessages, ListRuns, LocalAgent, LocalStore, LoggingStore, McpAuth, McpServer,
    McpTransport, MemoryStore, ModelChoice, Prompt, Run, RunEvent, RunStatus, RuntimeFilter,
    SendOptions, SettingSource, StoreRequest, SubAgent, SubAgentMcpServer,
};
use serde_json::{json, Value};

// ---- Quick start ----------------------------------------------------------

async fn quick_start() -> cursor_sdk::Result<()> {
    let client = Client::new();
    let agent = client
        .create_agent(AgentOptions::local(".").model("composer-2.5"))
        .await?;
    println!(
        "{}",
        agent.ask("Summarize what this repository does").await?
    );
    agent.close().await?;
    client.close().await?;
    Ok(())
}

async fn cloud_quick_start() -> cursor_sdk::Result<()> {
    let client = Client::new();
    let agent = client
        .create_agent(
            AgentOptions::new().cloud_options(
                CloudAgent::new("https://github.com/o/r")
                    .repositories([
                        CloudRepository::new("https://github.com/o/r").starting_ref("main")
                    ])
                    .auto_create_pr(true),
            ),
        )
        .await?;
    println!("{}", agent.ask("Add structured logging").await?);
    client.close().await?;
    Ok(())
}

async fn concurrency() -> cursor_sdk::Result<()> {
    let client = Client::new();
    let mut tasks = Vec::new();
    for path in ["/repo/a", "/repo/b"] {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client
                .prompt(
                    AgentOptions::local(path).model("composer-2.5"),
                    "Any TODOs?",
                )
                .await
        }));
    }
    for task in tasks {
        println!("{}", task.await.unwrap()?);
    }
    client.close().await?;
    Ok(())
}

// ---- Creating agents ------------------------------------------------------

async fn create_agents(client: &Client) -> cursor_sdk::Result<()> {
    let _local = client
        .create_agent(AgentOptions::local("/path/to/repo").model("composer-2.5"))
        .await?;
    let _cloud = client
        .create_agent(
            AgentOptions::new()
                .cloud_options(CloudAgent::new("https://github.com/o/r").auto_create_pr(true)),
        )
        .await?;
    let _no_repo = client
        .create_agent(AgentOptions::new().cloud_options(CloudAgent::no_repository()))
        .await?;
    let _idempotent = client
        .create_agent_idempotent(AgentOptions::cloud("https://github.com/o/r"), "key-1")
        .await?;
    Ok(())
}

fn agent_option_surface() {
    let _env = AgentOptions::new().cloud_options(
        CloudAgent::new("https://github.com/o/r").env_var("STAGING_API_TOKEN", "value"),
    );
    let _metadata = AgentOptions::new().cloud_options(
        CloudAgent::new("https://github.com/o/r")
            .metadata("end_user_id", "user-123")
            .metadata("ticket_id", "ENG-456"),
    );
    let _params =
        AgentOptions::local(".").model(ModelChoice::new("composer-2.5").with_param("fast", "true"));
    let _full = AgentOptions::local(".")
        .model("composer-2.5")
        .api_key("k")
        .name("agent")
        .agent_id("agent-1")
        .mode(AgentMode::Plan)
        .tools(["read", "grep"])
        .disallowed_tools(["shell"]);
    let _cloud_full = AgentOptions::new().cloud_options(
        CloudAgent::no_repository()
            .environment(CloudEnvironmentKind::Pool, "my-pool")
            .work_on_current_branch(true)
            .skip_reviewer_request(true)
            .open_as_cursor_github_app(false),
    );
    let _local_full = AgentOptions::new().model("m").local_options(
        LocalAgent::new(".")
            .dirs(["/other/root"])
            .setting_sources([SettingSource::Project, SettingSource::User])
            .sandbox(true)
            .auto_review(true)
            .store(LocalStore::Sqlite)
            .custom_tool(CustomTool::new("t", "d", json!({"type": "object"}))),
    );
}

// ---- Cursor Router --------------------------------------------------------

async fn router_discovery(client: &Client) -> cursor_sdk::Result<ModelChoice> {
    let models = client.models().await?;
    let router = models
        .iter()
        .find(|model| model.id == "auto-smart")
        .ok_or_else(|| Error::Config("Cursor Router is not available for this API key.".into()))?;
    let optimize_for = router
        .parameters
        .iter()
        .find(|parameter| parameter.id == "optimize_for")
        .ok_or_else(|| Error::Config("Router has no optimize_for parameter".into()))?;

    let requested = "balanced";
    if !optimize_for
        .values
        .iter()
        .any(|value| value.value == requested)
    {
        return Err(Error::Config(format!(
            "Router mode {requested:?} is not enabled"
        )));
    }
    Ok(ModelChoice::new(&router.id).with_param(&optimize_for.id, requested))
}

// ---- Sending and streaming ------------------------------------------------

async fn streaming(agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
    let mut run = agent.send("Find the bug in src/auth.rs").await?;
    while let Some(event) = run.next_event().await {
        match event? {
            RunEvent::Message(message) => match message.kind.as_str() {
                "assistant" => {
                    if let Some(text) = message.text() {
                        print!("{text}");
                    }
                }
                "tool_call" => eprintln!(
                    "[tool] {}: {}",
                    message.payload["name"], message.payload["status"]
                ),
                "status" => eprintln!("[status] {}", message.payload["status"]),
                _ => {}
            },
            RunEvent::Completed(outcome) => {
                println!("{} in {:?}", outcome.status, outcome.duration);
            }
            _ => {}
        }
    }
    Ok(())
}

async fn text_output(agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
    let mut run = agent.send("hi").await?;
    while let Some(chunk) = run.next_text().await {
        print!("{}", chunk?);
    }
    let _final_text = agent.send("Explain this module").await?.text().await?;
    Ok(())
}

async fn waiting(agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
    let outcome = agent.send("Refactor the parser").await?.wait().await?;
    println!("{}", outcome.status);
    println!("{}", outcome.text);
    println!("{:?}", outcome.model);
    println!("{:?}", outcome.duration);
    println!("{:?}", outcome.usage);
    println!("{:?}", outcome.git);
    println!("{}", outcome.run_id);
    println!("{}", outcome.agent_id);
    println!("{:?}", outcome.error_code);
    println!("{:?}", outcome.failure_message);
    println!("{:?}", outcome.created_at);
    let _: bool = outcome.status.is_terminal();
    let _: bool = outcome.status.is_success();

    if let Some(reason) = outcome.failure_reason() {
        eprintln!("run did not succeed: {reason}");
    }
    match outcome.usage {
        Some(usage) => println!(
            "total {}, in {}, out {}, cache r/w {}/{}, reasoning {:?}",
            usage.total_tokens,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            usage.reasoning_tokens,
        ),
        None => println!("no usage reported"),
    }
    for branch in &outcome.git {
        println!("{} {} {}", branch.repo_url, branch.branch, branch.pr_url);
    }
    Ok(())
}

async fn usage_stream_event(run: &mut Run) -> cursor_sdk::Result<()> {
    while let Some(event) = run.next_event().await {
        if let RunEvent::Message(message) = event? {
            if message.kind == "usage" {
                println!(
                    "turn used {} tokens",
                    message.payload["usage"]["totalTokens"]
                );
            }
        }
    }
    Ok(())
}

async fn run_state(client: &Client, run: &mut Run) -> cursor_sdk::Result<()> {
    println!("{:?}", run.run_id());
    println!("{}", run.agent_id());
    println!("{:?}", run.last_offset());
    println!("{:?}", run.outcome());
    let _document: Value = client.run_conversation("run_1").await?;
    run.cancel().await?;
    run.resume().await?;
    Ok(())
}

async fn streams_and_combinators(run: Run) {
    let _stream = run.into_stream();
}

async fn send_options(agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
    let _override = agent
        .send_with(
            "Plan the refactor",
            SendOptions::new().model(ModelChoice::new("composer-2.5").with_param("fast", "true")),
        )
        .await?;
    let _env = agent
        .send_with(
            "Deploy",
            SendOptions::new().cloud_env_var("DEPLOY_TOKEN", "token"),
        )
        .await?;
    let _mode = agent
        .send_with("Start building", SendOptions::new().mode(AgentMode::Agent))
        .await?;
    let _deltas = agent
        .send_with("Refactor", SendOptions::new().deltas(true).steps(true))
        .await?;
    let _forced = agent
        .send_with("Go", SendOptions::new().force(true))
        .await?;
    let _mcp = agent
        .send_with(
            "Go",
            SendOptions::new().mcp_server("docs", McpServer::http("https://x/mcp")),
        )
        .await?;
    let _idempotent = agent
        .send_idempotent("Go", SendOptions::new(), "send-key-1")
        .await?;
    Ok(())
}

async fn deltas_and_steps(run: &mut Run) -> cursor_sdk::Result<()> {
    while let Some(event) = run.next_event().await {
        match event? {
            RunEvent::Delta { kind, payload } if kind == "text-delta" => {
                print!("{}", payload["text"].as_str().unwrap_or_default());
            }
            RunEvent::Step { kind, payload } => eprintln!("[step] {kind} {payload}"),
            _ => {}
        }
    }
    Ok(())
}

fn prompts_and_images() -> cursor_sdk::Result<()> {
    let _plain: Prompt = "explain this".into();
    let _from_string: Prompt = String::from("explain").into();
    let _with_image = Prompt::text("What's in this screenshot?")
        .with_image(Image::from_file("shot.png")?)
        .with_image(Image::bytes(b"raw", "image/png").with_dimension(4, 2))
        .with_image(Image::url("https://example.com/a.png"));
    Ok(())
}

fn stream_message_helpers(message: cursor_sdk::StreamMessage) {
    let _: Option<String> = message.text();
    let _: bool = message.is_assistant();
    let _: Option<&str> = message.run_id();
    let _: Option<&str> = message.agent_id();
    let _: &str = &message.kind;
    let _: &Value = &message.payload;
}

// ---- Resuming and inspection ----------------------------------------------

async fn resuming(client: &Client) -> cursor_sdk::Result<()> {
    let agent = client
        .resume_agent("bc-abc123", AgentOptions::local(".").model("composer-2.5"))
        .await?;
    println!("{}", agent.ask("Also update the changelog").await?);
    let _bare = client.agent("agent-1");
    let _observed = client.observe_run("run_1", None).await?;
    let _observed_after = client.observe_run("run_1", Some("offset-1")).await?;
    let _from_agent = agent.observe("run_1", None).await?;
    Ok(())
}

async fn inspection(client: &Client) -> cursor_sdk::Result<()> {
    let page = client
        .list_agents(
            ListAgents::new()
                .runtime(RuntimeFilter::Local)
                .cwd(".")
                .limit(20),
        )
        .await?;
    for info in &page.items {
        println!("{} {:?} {}", info.id, info.status, info.name);
        println!(
            "{} {:?} {:?}",
            info.summary, info.last_modified, info.created_at
        );
        println!("{}", info.archived);
        match &info.runtime {
            AgentRuntime::Local { cwd } => println!("local {cwd}"),
            AgentRuntime::Cloud {
                env,
                repos,
                metadata,
            } => {
                println!("cloud {env:?} {repos:?} {metadata:?}")
            }
            _ => {}
        }
        let _: bool = info.status == AgentStatus::Running;
    }
    let _: bool = page.has_more();
    if let Some(cursor) = page.next_cursor.clone() {
        let _next = client.list_agents(ListAgents::new().cursor(cursor)).await?;
    }
    for _item in page {
        // Page<T> iterates directly.
    }

    let all = client
        .list_all_agents(
            ListAgents::new()
                .include_archived(true)
                .pr_url("https://pr")
                .api_key("k"),
        )
        .await?;
    let info = client.get_agent(&all[0].id).await?;
    let agent = client.agent(&info.id);
    let runs = agent
        .runs(ListRuns::new().limit(10).runtime(RuntimeFilter::Cloud))
        .await?;
    let messages = agent
        .messages(ListMessages::new().limit(50).offset(10))
        .await?;
    for message in &messages {
        println!(
            "{} {} {} {}",
            message.kind, message.uuid, message.agent_id, message.payload
        );
    }
    let _snapshot = client.get_run(&runs.items[0].run_id).await?;
    let _waited = client.wait_live_run(&runs.items[0].run_id).await?;
    client
        .cancel_run(&runs.items[0].run_id, Some(&info.id))
        .await?;
    let _conversation = agent.conversation("run_1").await?;
    let _info = agent.info().await?;
    Ok(())
}

async fn lifecycle(agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
    agent.archive().await?;
    agent.unarchive().await?;
    agent.delete().await?;
    agent.close().await?;
    agent.reload().await?;
    let guard = agent.close_on_drop();
    guard.close().await?;
    let _: &str = agent.id();
    let _: Option<&ModelChoice> = agent.model();
    let _: Option<&std::path::Path> = agent.cwd();
    let _: &Client = agent.client();
    Ok(())
}

async fn usage_and_artifacts(client: &Client, agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
    if client.version().await?.has_capability("agent.usage") {
        let usage = agent.usage().await?;
        println!("tokens: {}", usage.usage.total_tokens);
        if let Some(cost) = usage.cost {
            println!("charged: ${:.2}", cost.charged_cents / 100.0);
            println!("raw: {}", cost.raw_cost_cents);
        }
        for run in &usage.runs {
            println!("{} {} {:?}", run.run_id, run.usage.total_tokens, run.cost);
        }
    }
    let _run_usage = agent.run_usage("run_1").await?;

    for artifact in agent.artifacts().await? {
        println!(
            "{} ({} bytes) {}",
            artifact.path, artifact.size_bytes, artifact.updated_at
        );
    }
    let _bytes: Vec<u8> = agent.download_artifact("out/report.md").await?;
    let _written: u64 = agent
        .download_artifact_to("out/report.md", "review.md")
        .await?;
    Ok(())
}

// ---- Catalog --------------------------------------------------------------

async fn catalog(client: &Client) -> cursor_sdk::Result<()> {
    let me = client.me().await?;
    println!(
        "{} {} {} {} {} {}",
        me.api_key_name, me.user_id, me.email, me.first_name, me.last_name, me.created_at
    );

    let models = client.models().await?;
    for model in &models {
        println!(
            "{} — {} — {}",
            model.id, model.display_name, model.description
        );
        for parameter in &model.parameters {
            let values: Vec<&str> = parameter.values.iter().map(|v| v.value.as_str()).collect();
            println!(
                "    {} {} = {}",
                parameter.id,
                parameter.display_name,
                values.join(" | ")
            );
        }
        for variant in &model.variants {
            println!(
                "  {} {} {}",
                variant.display_name, variant.description, variant.is_default
            );
            println!("  {:?}", variant.params);
        }
    }
    if let Some(model) = models.iter().find(|m| m.id == "composer-2.5") {
        let choice: ModelChoice = model.choice();
        println!("{choice}");
    }

    for repository in client.repositories().await? {
        println!("{}", repository.url);
    }
    Ok(())
}

// ---- Client and bridge ----------------------------------------------------

fn builder_surface() -> Client {
    Client::builder()
        .api_key("k")
        .workspace("/path/to/repo")
        .bridge_binary("/opt/cursor-sdk-bridge/bin/cursor-sdk-bridge")
        .request_timeout(Duration::from_secs(60))
        .startup_timeout(Duration::from_secs(30))
        .shutdown_timeout(Duration::from_secs(5))
        .shutdown_grace(Duration::ZERO)
        .local_store(LocalStore::Jsonl {
            root_dir: "/var/lib/cursor-agents".into(),
        })
        .bridge_env("HTTP_PROXY", "http://proxy")
        .bridge_arg("--max-concurrent-agents")
        .verbose(true)
        .client_language("rust")
        .verify_on_connect(true)
        .build()
}

fn attached_client(token: &str) -> Client {
    Client::builder()
        .endpoint("http://127.0.0.1:43210", token)
        .build()
}

async fn bridge_lifecycle(client: &Client) -> cursor_sdk::Result<()> {
    println!("{}", client.ping().await?);
    let version = client.version().await?;
    println!("{} {}", version.bridge_version, version.protocol_version);
    println!("{:?}", version.capabilities);
    let _: bool = version.speaks_supported_protocol();
    let _: bool = version.has_capability("artifacts.chunked");

    println!("{}", client.endpoint().await?);
    if let Some(info) = client.bridge_info().await? {
        println!("{} {:?} {:?}", info.url, info.pid, info.server_version);
        println!("{:?} {:?}", info.workspace_ref, info.state_root);
        println!(
            "{:?} {:?}",
            info.max_concurrent_agents, info.max_message_bytes
        );
    }
    println!("{:?}", client.api_key());
    cursor_sdk::install_exit_guard();
    client.close().await?;
    Ok(())
}

async fn escape_hatch(client: &Client) -> cursor_sdk::Result<()> {
    use cursor_sdk::proto;
    let _response: proto::PingResponse = client
        .call_unary("SdkBridgeControlService", "Ping", &proto::PingRequest {})
        .await?;
    let _stream: cursor_sdk::ServerStream<proto::RunStreamMessage> = client
        .call_server_stream(
            "SdkAgentService",
            "ObserveRun",
            &proto::ObserveRunRequest {
                run_id: "r".into(),
                after_offset: None,
            },
        )
        .await?;
    Ok(())
}

// ---- MCP, subagents, tools ------------------------------------------------

fn mcp_servers() {
    let _options = AgentOptions::local(".")
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
            McpServer::stdio(
                "npx",
                ["-y", "@modelcontextprotocol/server-filesystem", "."],
            ),
        )
        .mcp_server(
            "sse",
            McpServer::Http {
                transport: McpTransport::Sse,
                url: "https://x/sse".into(),
                headers: Default::default(),
                auth: None,
            },
        );
}

fn subagents() {
    let _options = AgentOptions::local(".")
        .model("composer-2.5")
        .sub_agent(
            "code-reviewer",
            SubAgent {
                inherit_model: Some(true),
                ..SubAgent::new("Expert code reviewer.", "Review code for bugs.")
            },
        )
        .sub_agent(
            "test-writer",
            SubAgent {
                model: Some(ModelChoice::new("composer-2.5")),
                mcp_servers: vec![
                    SubAgentMcpServer::Named("docs".into()),
                    SubAgentMcpServer::Inline(Box::new(McpServer::http("https://x/mcp"))),
                ],
                ..SubAgent::new("Writes tests.", "Write comprehensive tests.")
            },
        );
}

fn custom_tools() -> Client {
    Client::builder()
        .register_tool(
            CustomTool::new(
                "deployment_status",
                "Look up the current deployment status for a service.",
                json!({
                    "type": "object",
                    "properties": {"service": {"type": "string"}},
                    "required": ["service"],
                }),
            )
            .with_output_schema(json!({"type": "object"})),
            |call| async move {
                let service = call
                    .require("service")?
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let _: Option<&Value> = call.arg("service");
                let _: Option<&str> = call.string_arg("service");
                let _: &str = &call.name;
                let _: &str = &call.agent_id;
                let _: Option<String> = call.tool_call_id.clone();
                Ok(json!({"service": service, "healthy": true}))
            },
        )
        .build()
}

async fn register_later(client: &Client) -> cursor_sdk::Result<()> {
    client
        .register_tool(
            CustomTool::new("later", "d", json!({"type": "object"})),
            |_call| async move { Ok(json!("done")) },
        )
        .await?;
    let _names: Vec<String> = client.tools().names();
    let _removed: bool = client.tools().unregister("later");
    let _empty: bool = client.tools().is_empty();
    Ok(())
}

// ---- Custom stores --------------------------------------------------------

struct MyStore;

impl AgentStore for MyStore {
    fn call<'a>(&'a self, request: StoreRequest) -> BoxFuture<'a, HandlerResult<Option<Value>>> {
        Box::pin(async move {
            let _: Option<&Value> = request.field("agentId");
            let _: Option<&str> = request.string_field("agentId");
            match (request.substore.as_str(), request.method.as_str()) {
                ("agents", "get") => Ok(None),
                ("agents", "create") => Ok(Some(request.record().clone())),
                _ => Ok(None),
            }
        })
    }
}

fn store_clients() {
    let _mine = Client::builder().agent_store(MyStore).build();
    let _memory = Client::builder().agent_store(MemoryStore::new()).build();
    let _logging = Client::builder()
        .agent_store(LoggingStore::new(MemoryStore::new()))
        .build();
}

// ---- Errors ---------------------------------------------------------------

async fn retrying(client: &Client) -> cursor_sdk::Result<()> {
    let mut delay = Duration::from_secs(1);
    for attempt in 0..3 {
        match client
            .prompt(
                AgentOptions::local(".").model("composer-2.5"),
                "Audit the middleware",
            )
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
                eprintln!("{error} (request {:?})", error.request_id());
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn agent_busy(agent: &cursor_sdk::Agent) -> cursor_sdk::Result<()> {
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
    Ok(())
}

fn error_surface(error: Error) {
    let _: Option<ErrorKind> = error.kind();
    let _: Option<&str> = error.request_id();
    let _: Option<Duration> = error.retry_after();
    let _: bool = error.is_retryable();
    let _: bool = error.is_auth();
    let _: bool = error.is_not_found();

    // Every documented ErrorKind exists.
    let _kinds = [
        ErrorKind::Unauthenticated,
        ErrorKind::PermissionDenied,
        ErrorKind::NotFound,
        ErrorKind::Validation,
        ErrorKind::RateLimited,
        ErrorKind::AgentBusy,
        ErrorKind::InvalidState,
        ErrorKind::Upstream,
        ErrorKind::Internal,
        ErrorKind::Cancelled,
        ErrorKind::Unknown,
    ];

    if let Error::Rpc(rpc) = error {
        let _: ErrorKind = rpc.kind;
        let _: &str = &rpc.connect_code;
        let _: i32 = rpc.sdk_error_code;
        let _: &str = &rpc.message;
        let _: Option<String> = rpc.request_id.clone();
        let _: Option<String> = rpc.help_url.clone();
        let _: Option<String> = rpc.provider.clone();
        let _: Option<Duration> = rpc.retry_after;
        let _: &str = &rpc.rpc;
        if let Some(limit) = rpc.rate_limit {
            let _: Option<u64> = limit.limit;
            let _: Option<u64> = limit.remaining;
            let _: Option<u64> = limit.reset_epoch_seconds;
        }
        let _: Option<&'static str> = rpc.sdk_error_code_name();
    }
}

fn bridge_error_surface(error: cursor_sdk::BridgeError) {
    match error {
        cursor_sdk::BridgeError::NotFound(_) => {}
        cursor_sdk::BridgeError::Spawn { .. } => {}
        cursor_sdk::BridgeError::ExitedBeforeReady { .. } => {}
        cursor_sdk::BridgeError::StartupTimeout { .. } => {}
        cursor_sdk::BridgeError::Handshake(_) => {}
        cursor_sdk::BridgeError::AuthToken { .. } => {}
        cursor_sdk::BridgeError::Manifest { .. } => {}
        _ => {}
    }
}

async fn manifest_surface(binary: &std::path::Path) {
    // The preflight runs internally, but the manifest is readable on its own.
    if let Some(manifest) = cursor_sdk::BridgeManifest::for_binary(binary).await {
        let _: Option<String> = manifest.bridge_version.clone();
        let _: Option<String> = manifest.sdk_version.clone();
        let _: Option<String> = manifest.os.clone();
        let _: Option<String> = manifest.arch.clone();
        let _: Option<String> = manifest.protocol.clone();
        let _: Option<String> = manifest.entrypoint.clone();
        let _: Option<String> = manifest.distribution.clone();
        let _: Option<String> = manifest.runtime.clone();
        let _: bool = manifest.speaks_supported_protocol();
    }
    let _: Option<std::path::PathBuf> = cursor_sdk::BridgeManifest::path_for_binary(binary);
    let _: &str = cursor_sdk::manifest::host_os();
    let _: &str = cursor_sdk::manifest::host_arch();
}

fn constants() {
    let _: &str = cursor_sdk::PROTOCOL_VERSION;
    let _: &str = cursor_sdk::CONTRACT_SDK_VERSION;
}

#[test]
fn the_documented_surface_compiles() {
    // The compile is the assertion; nothing above is ever called.
}
