//! Verification probe: force the bridge to issue `list` against a custom store.
//!
//! A plain agent turn never lists, so this creates an agent, runs one short
//! turn, then asks the SDK to enumerate agents and runs — which routes through
//! the store's `list`. If the SDK gets records back, the store's list envelope
//! is the shape the bridge expects.

use std::sync::{Arc, Mutex};

use cursor_sdk::callback::BoxFuture;
use cursor_sdk::{
    AgentOptions, AgentStore, Client, HandlerResult, ListAgents, ListRuns, MemoryStore,
    RuntimeFilter, StoreRequest,
};
use serde_json::Value as JsonValue;

/// Records every operation and the exact bytes returned for `list`.
struct ProbeStore {
    inner: MemoryStore,
    seen: Arc<Mutex<Vec<String>>>,
    lists: Arc<Mutex<Vec<(String, JsonValue, JsonValue)>>>,
}

impl AgentStore for ProbeStore {
    fn call<'a>(
        &'a self,
        request: StoreRequest,
    ) -> BoxFuture<'a, HandlerResult<Option<JsonValue>>> {
        let label = format!("{}.{}", request.substore.as_str(), request.method.as_str());
        let is_list = request.method.as_str() == "list";
        let input = request.input.clone();
        let seen = Arc::clone(&self.seen);
        let lists = Arc::clone(&self.lists);

        Box::pin(async move {
            seen.lock().unwrap().push(label.clone());
            let result = self.inner.call(request).await;
            if is_list {
                let output = match &result {
                    Ok(Some(value)) => value.clone(),
                    Ok(None) => JsonValue::Null,
                    Err(error) => JsonValue::String(error.to_string()),
                };
                lists.lock().unwrap().push((label, input, output));
            }
            result
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let lists = Arc::new(Mutex::new(Vec::new()));

    let client = Client::builder()
        .workspace(&workspace)
        .agent_store(ProbeStore {
            inner: MemoryStore::new(),
            seen: Arc::clone(&seen),
            lists: Arc::clone(&lists),
        })
        .build();

    let agent = client
        .create_agent(AgentOptions::local(&workspace).model("composer-2"))
        .await?;
    println!("created {}", agent.id());

    // One short turn, so there is a run record to list as well.
    println!(
        "turn: {}",
        agent.ask("Reply with the single word: ok.").await?
    );

    // These are what force `list` through the store.
    let agents = client
        .list_agents(
            ListAgents::new()
                .runtime(RuntimeFilter::Local)
                .cwd(&workspace),
        )
        .await?;
    println!("\nlist_agents returned {} agent(s)", agents.items.len());
    for info in &agents.items {
        println!("  {} {:?}", info.id, info.status);
    }

    let runs = agent
        .runs(ListRuns::new().runtime(RuntimeFilter::Local))
        .await?;
    println!("list_runs returned {} run(s)", runs.items.len());
    for run in &runs.items {
        println!("  {} {}", run.run_id, run.status);
    }

    // Replaying a finished run reads the durable event log, which is the only
    // thing that issues `runEvents.list`.
    if let Some(run) = runs.items.first() {
        let mut replay = agent.observe(&run.run_id, None).await?;
        let mut events = 0;
        while let Some(event) = replay.next_event().await {
            event?;
            events += 1;
        }
        println!("observe_run replayed {events} event(s)");
    }

    println!("\n--- list operations the bridge issued ---");
    // Copy out and release the lock: it must not be held across an await.
    let observed = lists.lock().unwrap().clone();
    if observed.is_empty() {
        println!("(none — the bridge never issued a list)");
    }
    for (label, input, output) in &observed {
        println!("\n{label}");
        println!("  in : {input}");
        let rendered = output.to_string();
        println!("  out: {}", &rendered[..rendered.len().min(400)]);
    }

    println!("\n--- verdict ---");
    let store_had_agent = !agents.items.is_empty();
    println!(
        "agents.list round-tripped through our envelope: {}",
        if store_had_agent { "YES" } else { "NO" }
    );

    let mut counts = std::collections::BTreeMap::new();
    for label in seen.lock().unwrap().iter() {
        *counts.entry(label.clone()).or_insert(0) += 1;
    }
    println!("\nall store ops: {counts:?}");

    agent.close().await?;
    client.close().await?;
    Ok(())
}
