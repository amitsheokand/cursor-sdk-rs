//! Own the agent's durable state in this process.
//!
//! With a custom store the bridge stops persisting local agent state itself and
//! forwards every operation to an [`AgentStore`]. This example wraps the
//! built-in `MemoryStore` in `LoggingStore`, which is exactly the workflow the
//! bridge docs recommend for building a real one: run a turn, read the traffic.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! RUST_LOG=cursor_sdk::store=debug cargo run --example custom_store
//! ```

use std::sync::Arc;

use cursor_sdk::{
    AgentOptions, AgentStore, Client, HandlerResult, LoggingStore, MemoryStore, StoreRequest,
};
use serde_json::Value as JsonValue;

/// A store that counts what the bridge asks of it, on top of a real one.
struct CountingStore {
    inner: LoggingStore<MemoryStore>,
    operations: Arc<std::sync::Mutex<Vec<String>>>,
}

impl AgentStore for CountingStore {
    fn call<'a>(
        &'a self,
        request: StoreRequest,
    ) -> cursor_sdk::callback::BoxFuture<'a, HandlerResult<Option<JsonValue>>> {
        let label = format!("{}.{}", request.substore.as_str(), request.method.as_str());
        if let Ok(mut operations) = self.operations.lock() {
            operations.push(label);
        }
        self.inner.call(request)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,cursor_sdk::store=debug".into()),
        )
        .init();

    let operations = Arc::new(std::sync::Mutex::new(Vec::new()));

    // The store must be configured before the bridge launches: agents can load
    // state before any RPC arrives, so there is no way to add one later.
    let client = Client::builder()
        .agent_store(CountingStore {
            inner: LoggingStore::new(MemoryStore::new()),
            operations: Arc::clone(&operations),
        })
        .build();

    let agent = client
        .create_agent(AgentOptions::local(std::env::current_dir()?).model("composer-2.5"))
        .await?;
    let _guard = agent.close_on_drop();

    println!("{}", agent.ask("Say hello in five words.").await?);

    println!("\nstore operations the bridge performed:");
    for operation in operations.lock().unwrap().iter() {
        println!("  {operation}");
    }

    client.close().await?;
    Ok(())
}
