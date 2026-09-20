//! Identity and catalog: who the key belongs to, and what it can use.
//!
//! The cheapest way to check that a key, a bridge binary, and this SDK all
//! agree — it starts no agent and spends nothing.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example catalog
//! ```

use cursor_sdk::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let client = Client::new();

    let version = client.version().await?;
    println!(
        "bridge {} speaking {} ({} capabilities)",
        version.bridge_version,
        version.protocol_version,
        version.capabilities.len()
    );
    if let Some(info) = client.bridge_info().await? {
        println!("endpoint {}  workspace {:?}", info.url, info.workspace_ref);
    }

    let user = client.me().await?;
    println!(
        "\nauthenticated as {} <{}> via key {:?}",
        user.user_id, user.email, user.api_key_name
    );

    println!("\nmodels:");
    for model in client.models().await? {
        print!("  {:<24}", model.id);
        if !model.display_name.is_empty() {
            print!(" {}", model.display_name);
        }
        println!();
        for parameter in &model.parameters {
            let values: Vec<&str> = parameter.values.iter().map(|v| v.value.as_str()).collect();
            println!("      {} = {}", parameter.id, values.join(" | "));
        }
    }

    match client.repositories().await {
        Ok(repositories) => {
            println!(
                "\nrepositories usable with cloud agents: {}",
                repositories.len()
            );
            for repository in repositories.iter().take(10) {
                println!("  {}", repository.url);
            }
        }
        // Repository listing needs the account to have connected a git host.
        Err(error) => println!("\nrepositories unavailable: {error}"),
    }

    client.close().await?;
    Ok(())
}
