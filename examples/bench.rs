//! Minimal community-style bench runner: plain `cursor-sdk` API only.
//!
//! No seat modules, no catalog validation, no retry, no session. This is
//! the community-Rust arm of the three-way bench: what upstream
//! `cursor-sdk-rs` gives you out of the box.
//!
//! Usage: `bench <cwd> <model> <fast:true|false> <prompt>`
//! Prints one JSON object on stdout; errors go to stderr with exit 1.

use std::collections::BTreeMap;
use std::time::Instant;

use cursor_sdk::{AgentOptions, Client, ModelChoice};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("bench: {message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let cwd = args.next().ok_or("usage: bench <cwd> <model> <fast> <prompt>")?;
    let model = args.next().ok_or("usage: bench <cwd> <model> <fast> <prompt>")?;
    let fast = args.next().ok_or("usage: bench <cwd> <model> <fast> <prompt>")?;
    let prompt: Vec<String> = args.collect();
    if prompt.is_empty() {
        return Err("usage: bench <cwd> <model> <fast> <prompt>".to_string());
    }
    let fast = match fast.as_str() {
        "true" => true,
        "false" => false,
        _ => return Err("fast must be true|false".to_string()),
    };

    let started = Instant::now();
    let client = Client::new();
    let mut choice = ModelChoice::new(&model);
    if !fast {
        choice = choice.with_param("fast", "false");
    }
    let agent = client
        .create_agent(
            AgentOptions::local(&cwd)
                .model(choice)
                .disallowed_tools(["task"]),
        )
        .await
        .map_err(|e| e.to_string())?;
    let outcome = agent
        .send(&prompt.join(" "))
        .await
        .map_err(|e| e.to_string())?
        .wait()
        .await
        .map_err(|e| e.to_string())?;
    // Usage prefers the stream result (always reported); the GetUsage
    // RPC is account-gated and must not fail the run. Some accounts
    // lack it entirely.
    let stream_usage = outcome.usage.clone();
    let usage = match stream_usage {
        Some(usage) => Some(usage),
        None => agent.usage().await.ok().map(|full| full.usage),
    };
    let echoed = agent.model().cloned();
    let _ = agent.close().await;
    client.close().await.map_err(|e| e.to_string())?;

    let mut usage_map = BTreeMap::new();
    if let Some(usage) = usage {
        usage_map.insert("input_tokens", usage.input_tokens);
        usage_map.insert("output_tokens", usage.output_tokens);
        usage_map.insert("cache_read_tokens", usage.cache_read_tokens);
        usage_map.insert("cache_write_tokens", usage.cache_write_tokens);
        usage_map.insert("total_tokens", usage.total_tokens);
    }
    let usage_json = if usage_map.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::to_value(&usage_map).expect("usage map")
    };
    println!(
        "{}",
        serde_json::json!({
            "status": outcome.status.to_string(),
            "text": outcome.text,
            "run_id": outcome.run_id,
            "agent_id": outcome.agent_id,
            "model": echoed.map(|m| serde_json::json!({"id": m.id, "params": m.params})),
            "usage": usage_json,
            "wall_ms": started.elapsed().as_millis() as u64,
        })
    );
    Ok(())
}
