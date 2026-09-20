//! The shortest path from nothing to an answer.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example quickstart -- "What does this repository do?"
//! ```

use cursor_sdk::{AgentOptions, Client};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "In one sentence, what does this repository do?".to_string());
    let workspace = std::env::current_dir()?;

    // Nothing is spawned until the first call that needs the bridge.
    let client = Client::new();

    let answer = client
        .prompt(
            AgentOptions::local(&workspace).model("composer-2.5"),
            prompt.as_str(),
        )
        .await?;

    println!("{answer}");

    client.close().await?;
    Ok(())
}
