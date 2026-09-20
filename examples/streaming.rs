//! Stream a turn as it happens: assistant text, tool calls, and the outcome.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example streaming -- "Add a doc comment to the main entry point."
//! ```

use std::io::Write;

use cursor_sdk::{AgentOptions, Client, RunEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "List the files in this directory and summarize them.".to_string());

    let client = Client::new();
    // Kill the bridge if this program is interrupted, rather than leaking it.
    cursor_sdk::install_exit_guard();

    let agent = client
        .create_agent(AgentOptions::local(std::env::current_dir()?).model("composer-2.5"))
        .await?;
    let _guard = agent.close_on_drop();

    println!(
        "agent {} on {}",
        agent.id(),
        agent.model().map(|m| m.id.as_str()).unwrap_or("?")
    );

    let mut run = agent.send(prompt.as_str()).await?;

    while let Some(event) = run.next_event().await {
        match event? {
            RunEvent::Message(message) => match message.kind.as_str() {
                "assistant" => {
                    if let Some(text) = message.text() {
                        print!("{text}");
                        std::io::stdout().flush()?;
                    }
                }
                "tool_call" => {
                    let name = message.payload["name"].as_str().unwrap_or("tool");
                    let status = message.payload["status"].as_str().unwrap_or("");
                    eprintln!("\n  [{name} {status}]");
                }
                "thinking" => eprintln!("\n  [thinking]"),
                _ => {}
            },
            RunEvent::Completed(outcome) => {
                println!("\n\n--- {} in {:?} ---", outcome.status, outcome.duration);
                if let Some(reason) = outcome.failure_reason() {
                    eprintln!("failed: {reason}");
                }
                if let Some(usage) = outcome.usage {
                    eprintln!(
                        "tokens: {} in, {} out",
                        usage.input_tokens, usage.output_tokens
                    );
                }
            }
            // Deltas and steps only arrive when SendOptions asks for them.
            RunEvent::Delta { .. } | RunEvent::Step { .. } => {}
            _ => {}
        }
    }

    client.close().await?;
    Ok(())
}
