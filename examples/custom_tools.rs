//! Let an agent call Rust functions.
//!
//! The SDK runs a loopback Connect server; the bridge calls back into it with
//! a bearer token this process chose. Declaring a tool and implementing it are
//! one step here — `register_tool` does both.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example custom_tools
//! ```

use cursor_sdk::{AgentOptions, Client, CustomTool};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,cursor_sdk::callback=debug".into()),
        )
        .init();

    let client = Client::builder()
        .register_tool(
            CustomTool::new(
                "deployment_status",
                "Look up the current deployment status of one of our services. \
                 Use this instead of guessing or reading config files.",
                json!({
                    "type": "object",
                    "properties": {
                        "service": {
                            "type": "string",
                            "description": "The service name, for example \"checkout\".",
                        },
                    },
                    "required": ["service"],
                }),
            ),
            |call| async move {
                // Anything async goes here: a database, an HTTP call, a queue.
                let service = call
                    .require("service")?
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(json!({
                    "service": service,
                    "environment": "production",
                    "version": "2026.9.3",
                    "healthy": true,
                }))
            },
        )
        .register_tool(
            CustomTool::new(
                "round_trip_time_ms",
                "Measure the round-trip time to a host in milliseconds.",
                json!({
                    "type": "object",
                    "properties": {"host": {"type": "string"}},
                    "required": ["host"],
                }),
            ),
            |call| async move {
                let host = call.string_arg("host").unwrap_or("localhost").to_string();
                // A scalar return is fine: the SDK wraps it as {"value": …},
                // because a tool result is a protobuf Struct.
                Ok(json!(format!("{host}: 12ms")))
            },
        )
        .build();

    println!("registered tools: {:?}", client.tools().names());

    let agent = client
        .create_agent(AgentOptions::local(std::env::current_dir()?).model("composer-2.5"))
        .await?;
    let _guard = agent.close_on_drop();

    let answer = agent
        .ask("Using your tools, tell me the deployed version of the checkout service.")
        .await?;
    println!("\n{answer}");

    client.close().await?;
    Ok(())
}
