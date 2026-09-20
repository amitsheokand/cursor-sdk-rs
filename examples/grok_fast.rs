//! Cursor Grok 4.6 in fast mode with low reasoning effort.
//!
//! This is the model-parameter example. A parameterized model is one whose
//! catalog entry lists `parameters`; you pick values from those lists and pass
//! them on the model selection. Grok 4.6 exposes two:
//!
//! ```text
//! grok-4.6    Cursor Grok 4.6
//!     effort = low | medium | high | xhigh
//!     fast   = false | true
//! ```
//!
//! Note that `effort` *is* the thinking control on this model — there is no
//! separate `thinking` toggle. `thinking = false | true` belongs to the
//! Claude-family models. Which parameter means "how hard should it think"
//! genuinely varies by family: `effort` here, `reasoning` on the GPT models,
//! `reasoning_effort` on Gemini Flash, `thinking` on Claude.
//!
//! That matters more than it looks, because **the backend does not validate
//! parameter ids**. Sending `thinking=true` to Grok 4.6 is accepted and
//! silently ignored — the run succeeds and you simply never get the setting you
//! asked for. Nothing downstream will tell you. Checking the selection against
//! the catalog, as `resolve()` below does, is the only thing that catches a
//! parameter borrowed from the wrong model family or a plain typo.
//!
//! Leaving a parameter off is also not "the default you had in mind": the run
//! uses that parameter's first allowed value. Pass the ones you care about.
//!
//! ```text
//! export CURSOR_API_KEY=...
//! cargo run --example grok_fast -- "Where is the retry logic in this repo?"
//! ```

use std::io::Write;

use cursor_sdk::{AgentOptions, Client, Model, ModelChoice, RunEvent};

const MODEL: &str = "grok-4.6";
const FAST: (&str, &str) = ("fast", "true");
const EFFORT: (&str, &str) = ("effort", "low");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "In two sentences, what does this repository do?".to_string());

    let client = Client::new();

    // The catalog is account- and team-specific, so confirm the selection is
    // actually available before spending a turn discovering it isn't.
    let models = client.models().await?;
    let model = match resolve(&models) {
        Ok(model) => model,
        Err(reason) => {
            eprintln!("{reason}\n");
            eprintln!("Models available to this API key:");
            for model in &models {
                eprintln!("  {:<24} {}", model.id, model.display_name);
            }
            client.close().await?;
            return Ok(());
        }
    };

    println!("requesting {model}");

    let agent = client
        .create_agent(
            AgentOptions::local(std::env::current_dir()?)
                .model(model)
                .name("grok-fast-low-effort"),
        )
        .await?;
    let _guard = agent.close_on_drop();

    let mut run = agent.send(prompt.as_str()).await?;
    while let Some(event) = run.next_event().await {
        match event? {
            RunEvent::Message(message) => {
                if let Some(text) = message.is_assistant().then(|| message.text()).flatten() {
                    print!("{text}");
                    std::io::stdout().flush()?;
                }
            }
            RunEvent::Completed(outcome) => {
                println!("\n\n--- {} in {:?} ---", outcome.status, outcome.duration);
                // The resolved selection is what actually ran, which is the
                // honest place to confirm the parameters took effect.
                match &outcome.model {
                    Some(model) => println!("model: {model}"),
                    None => println!("model: not reported"),
                }
                if let Some(usage) = outcome.usage {
                    println!(
                        "tokens: {} in, {} out{}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage
                            .reasoning_tokens
                            .map(|count| format!(", {count} reasoning"))
                            .unwrap_or_default(),
                    );
                }
                if let Some(reason) = outcome.failure_reason() {
                    eprintln!("failed: {reason}");
                }
            }
            _ => {}
        }
    }

    client.close().await?;
    Ok(())
}

/// Build the selection, checking the model and every parameter value against
/// the catalog first.
fn resolve(models: &[Model]) -> Result<ModelChoice, String> {
    let model = models
        .iter()
        .find(|model| model.id == MODEL)
        .ok_or_else(|| format!("{MODEL} is not available to this API key."))?;

    let mut choice = model.choice();
    for (id, value) in [FAST, EFFORT] {
        let parameter = model
            .parameters
            .iter()
            .find(|parameter| parameter.id == id)
            .ok_or_else(|| {
                format!(
                    "{MODEL} has no {id:?} parameter. It accepts: {}",
                    names(model)
                )
            })?;

        if !parameter
            .values
            .iter()
            .any(|allowed| allowed.value == value)
        {
            let allowed: Vec<&str> = parameter
                .values
                .iter()
                .map(|allowed| allowed.value.as_str())
                .collect();
            return Err(format!(
                "{MODEL} does not accept {id}={value:?}. Allowed: {}",
                allowed.join(" | ")
            ));
        }
        choice = choice.with_param(id, value);
    }
    Ok(choice)
}

fn names(model: &Model) -> String {
    let ids: Vec<&str> = model
        .parameters
        .iter()
        .map(|parameter| parameter.id.as_str())
        .collect();
    if ids.is_empty() {
        "no parameters".to_string()
    } else {
        ids.join(", ")
    }
}
