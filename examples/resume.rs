//! Survive a dropped stream, and pick a conversation back up later.
//!
//! Two independent facts make this work:
//!
//! * Dropping a `Send` stream does not cancel the run. `Run::resume` reconnects
//!   to the durable event log, and `Run::wait` falls back to `WaitLiveRun`.
//! * An agent id outlives the process. `Client::resume_agent` re-attaches to
//!   the same conversation in a later run of your program.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example resume                 # starts a conversation
//! cargo run --example resume -- <agent-id>   # continues it
//! ```

use cursor_sdk::{AgentOptions, Client, RunEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let client = Client::new();
    let workspace = std::env::current_dir()?;
    let options = AgentOptions::local(&workspace).model("composer-2.5");

    let (agent, prompt) = match std::env::args().nth(1) {
        Some(agent_id) => {
            println!("resuming agent {agent_id}");
            (
                client.resume_agent(agent_id, options).await?,
                "What did I ask you a moment ago?",
            )
        }
        None => (
            client.create_agent(options).await?,
            "Remember the number 41. Reply with just the number.",
        ),
    };

    let mut run = agent.send(prompt).await?;

    // Consume a few events, then pretend the connection dropped.
    let mut seen = 0;
    while let Some(event) = run.next_event().await {
        let event = event?;
        seen += 1;
        if let RunEvent::Message(message) = &event {
            if let Some(text) = message.text() {
                print!("{text}");
            }
        }
        if seen == 2 {
            println!("\n[simulating a dropped connection after {seen} events]");
            break;
        }
    }

    // Reconnect to the durable log. Because this run was streaming live, the
    // replay starts from the beginning: live offsets are not valid ObserveRun
    // resume points, so events are re-delivered and de-duplicated by the caller.
    if run.run_id().is_some() {
        run.resume().await?;
        println!("[resumed run {}]", run.run_id().unwrap_or_default());
    }

    let outcome = run.wait().await?;
    println!("\n{}: {}", outcome.status, outcome.text);
    println!(
        "\nrun this again with:\n  cargo run --example resume -- {}",
        agent.id()
    );

    client.close().await?;
    Ok(())
}
