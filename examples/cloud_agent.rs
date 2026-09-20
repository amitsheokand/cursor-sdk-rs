//! Run an agent in Cursor's cloud against a git repository.
//!
//! Cloud agents work on repositories rather than a local directory, can open a
//! pull request when they finish, and are the only runtime that reports billed
//! usage or produces artifacts.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example cloud_agent -- https://github.com/you/repo "Fix the flaky test."
//! ```

use cursor_sdk::{AgentOptions, Client, CloudAgent, CloudRepository, RunEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let repository = args
        .next()
        .ok_or("usage: cloud_agent <repo-url> [prompt]")?;
    let prompt = args
        .next()
        .unwrap_or_else(|| "Summarize what this repository does.".to_string());

    let client = Client::new();

    // A cloud agent does not need an explicit model, unlike a local one.
    let agent = client
        .create_agent(
            AgentOptions::new().cloud_options(
                CloudAgent::new(&repository)
                    .repositories([CloudRepository::new(&repository).starting_ref("main")])
                    .auto_create_pr(false)
                    .metadata("source", "cursor-sdk-rust-example"),
            ),
        )
        .await?;

    println!("cloud agent {}", agent.id());

    let mut run = agent.send(prompt.as_str()).await?;
    while let Some(event) = run.next_event().await {
        match event? {
            RunEvent::Message(message) => {
                if let Some(text) = message.text() {
                    print!("{text}");
                }
            }
            RunEvent::Completed(outcome) => {
                println!("\n\n{} in {:?}", outcome.status, outcome.duration);
                for branch in &outcome.git {
                    println!("  {} -> {}", branch.branch, branch.pr_url);
                }
            }
            _ => {}
        }
    }

    // Artifacts and usage are cloud-only.
    for artifact in agent.artifacts().await.unwrap_or_default() {
        println!("artifact {} ({} bytes)", artifact.path, artifact.size_bytes);
    }
    match agent.usage().await {
        Ok(usage) => println!(
            "billed {} tokens, {:.2} cents",
            usage.usage.total_tokens,
            usage.cost.map(|cost| cost.charged_cents).unwrap_or(0.0),
        ),
        Err(error) => println!("usage unavailable: {error}"),
    }

    client.close().await?;
    Ok(())
}
