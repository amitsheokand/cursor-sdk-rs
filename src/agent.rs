//! The agent handle: the public API for one conversation.
//!
//! An [`Agent`] is a cheap handle around an `agent_id`. It remembers the
//! working directory it was created with, because several RPCs route by `cwd`,
//! and the model the bridge selected.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::client::{Client, ListMessages, ListRuns};
use crate::error::Result;
use crate::options::{Prompt, SendOptions};
use crate::run::Run;
use crate::types::{AgentInfo, AgentMessage, AgentUsage, Artifact, ModelChoice, Page, RunOutcome};

/// A conversation with a Cursor agent.
///
/// Created by [`Client::create_agent`], [`Client::resume_agent`], or
/// [`Client::agent`].
#[derive(Debug, Clone)]
pub struct Agent {
    client: Client,
    agent_id: String,
    model: Option<ModelChoice>,
    cwd: Option<PathBuf>,
}

impl Agent {
    pub(crate) fn new(
        client: Client,
        agent_id: String,
        model: Option<ModelChoice>,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            agent_id,
            model,
            cwd,
        }
    }

    /// The agent's id. Pass it to [`Client::resume_agent`] in a later process.
    pub fn id(&self) -> &str {
        &self.agent_id
    }

    /// The model the bridge selected, when it reported one.
    pub fn model(&self) -> Option<&ModelChoice> {
        self.model.as_ref()
    }

    /// The working directory this agent was created with, for local agents.
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The client this agent belongs to.
    pub fn client(&self) -> &Client {
        &self.client
    }

    // ---- sending ----------------------------------------------------------

    /// Send a message and start streaming the run.
    ///
    /// Returns as soon as the stream is open, before the agent has finished —
    /// consume [`Run`] incrementally, or call [`Run::wait`].
    pub async fn send(&self, prompt: impl Into<Prompt>) -> Result<Run> {
        self.send_with(prompt, SendOptions::new()).await
    }

    /// Send a message with per-turn options.
    pub async fn send_with(&self, prompt: impl Into<Prompt>, options: SendOptions) -> Result<Run> {
        self.send_inner(prompt.into(), options, None).await
    }

    /// Send a message with an idempotency key, so a retry cannot start a
    /// second run. Cloud agents only.
    pub async fn send_idempotent(
        &self,
        prompt: impl Into<Prompt>,
        options: SendOptions,
        idempotency_key: impl Into<String>,
    ) -> Result<Run> {
        self.send_inner(prompt.into(), options, Some(idempotency_key.into()))
            .await
    }

    async fn send_inner(
        &self,
        prompt: Prompt,
        options: SendOptions,
        idempotency_key: Option<String>,
    ) -> Result<Run> {
        let stream = self
            .client
            .send_stream(&self.agent_id, &prompt, &options, idempotency_key)
            .await?;
        Ok(Run::live(
            self.client.clone(),
            self.agent_id.clone(),
            stream,
        ))
    }

    /// Send a message and wait for the final assistant text.
    ///
    /// The blocking shape, for callers that do not want the stream.
    pub async fn ask(&self, prompt: impl Into<Prompt>) -> Result<String> {
        self.send(prompt).await?.text().await
    }

    /// Replay a run's durable events, optionally resuming after an offset.
    ///
    /// Only pass an offset a previous `ObserveRun` produced. To recover from a
    /// dropped live stream, use [`Run::resume`], which applies that rule for
    /// you.
    pub async fn observe(
        &self,
        run_id: impl Into<String>,
        after_offset: Option<&str>,
    ) -> Result<Run> {
        let run_id = run_id.into();
        let stream = self
            .client
            .observe_run_stream(&run_id, after_offset)
            .await?;
        Ok(Run::durable(
            self.client.clone(),
            self.agent_id.clone(),
            run_id,
            stream,
        ))
    }

    // ---- lifecycle --------------------------------------------------------

    /// Release the agent's local resources.
    ///
    /// Durable state survives: this is not a delete.
    pub async fn close(&self) -> Result<()> {
        self.client
            .agent_lifecycle("CloseAgent", &self.agent_id, self.cwd.as_deref())
            .await
    }

    /// Reload the agent's durable state from its store, without sending
    /// anything.
    pub async fn reload(&self) -> Result<()> {
        self.client
            .agent_lifecycle("ReloadAgent", &self.agent_id, self.cwd.as_deref())
            .await
    }

    /// Hide the agent from default listings.
    pub async fn archive(&self) -> Result<()> {
        self.client
            .agent_lifecycle("ArchiveAgent", &self.agent_id, self.cwd.as_deref())
            .await
    }

    /// Restore an archived agent.
    pub async fn unarchive(&self) -> Result<()> {
        self.client
            .agent_lifecycle("UnarchiveAgent", &self.agent_id, self.cwd.as_deref())
            .await
    }

    /// Permanently delete the agent and its durable data.
    pub async fn delete(&self) -> Result<()> {
        self.client
            .agent_lifecycle("DeleteAgent", &self.agent_id, self.cwd.as_deref())
            .await
    }

    // ---- inspection -------------------------------------------------------

    /// The agent's current metadata.
    pub async fn info(&self) -> Result<AgentInfo> {
        self.client.get_agent(&self.agent_id).await
    }

    /// One page of this agent's runs.
    pub async fn runs(&self, filter: ListRuns) -> Result<Page<RunOutcome>> {
        self.client
            .list_runs(&self.agent_id, &filter, self.cwd.as_deref())
            .await
    }

    /// The messages recorded for this agent.
    pub async fn messages(&self, filter: ListMessages) -> Result<Vec<AgentMessage>> {
        self.client
            .list_agent_messages(&self.agent_id, &filter, self.cwd.as_deref())
            .await
    }

    /// The raw conversation document for one of this agent's runs.
    pub async fn conversation(&self, run_id: impl Into<String>) -> Result<JsonValue> {
        self.client.run_conversation(run_id).await
    }

    /// Cancel an in-flight run of this agent.
    pub async fn cancel_run(&self, run_id: &str) -> Result<()> {
        self.client.cancel_run(run_id, Some(&self.agent_id)).await
    }

    // ---- artifacts and usage ----------------------------------------------

    /// The files this cloud agent produced.
    pub async fn artifacts(&self) -> Result<Vec<Artifact>> {
        self.client.list_artifacts(&self.agent_id).await
    }

    /// Download an artifact into memory.
    ///
    /// The bytes arrive in chunks; for a large artifact prefer
    /// [`Agent::download_artifact_to`], which never holds the whole file.
    pub async fn download_artifact(&self, path: &str) -> Result<Vec<u8>> {
        let mut stream = self
            .client
            .download_artifact_stream(&self.agent_id, path)
            .await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?.data);
        }
        Ok(bytes)
    }

    /// Stream an artifact straight to a file.
    pub async fn download_artifact_to(
        &self,
        path: &str,
        destination: impl AsRef<Path>,
    ) -> Result<u64> {
        use tokio::io::AsyncWriteExt;

        let destination = destination.as_ref();
        if let Some(parent) = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(destination).await?;
        let mut stream = self
            .client
            .download_artifact_stream(&self.agent_id, path)
            .await?;
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk.data).await?;
            written += chunk.data.len() as u64;
        }
        file.flush().await?;
        Ok(written)
    }

    /// Billed token usage and cost for this agent.
    ///
    /// Cloud agents only; a local agent fails with a cloud-only error. Gate on
    /// the bridge's `agent.usage` capability when you need to know in advance.
    pub async fn usage(&self) -> Result<AgentUsage> {
        self.client.usage(&self.agent_id, None).await
    }

    /// Billed usage for one of this agent's runs.
    pub async fn run_usage(&self, run_id: &str) -> Result<AgentUsage> {
        self.client.usage(&self.agent_id, Some(run_id)).await
    }
}

/// Closes the agent when the guard is dropped, from a synchronous scope.
///
/// [`Agent::close`] is async, so it cannot run in `Drop`. This guard spawns the
/// close on the current runtime instead, which covers the common "close when
/// this function returns, including on the error path" case.
///
/// ```no_run
/// # async fn demo(client: &cursor_sdk::Client) -> cursor_sdk::Result<()> {
/// # use cursor_sdk::AgentOptions;
/// let agent = client.create_agent(AgentOptions::local(".").model("composer-2.5")).await?;
/// let _guard = agent.close_on_drop();
/// // ... anything from here on, including `?`, still closes the agent.
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct AgentGuard {
    agent: Option<Agent>,
}

impl Agent {
    /// A guard that closes this agent when it is dropped.
    pub fn close_on_drop(&self) -> AgentGuard {
        AgentGuard {
            agent: Some(self.clone()),
        }
    }
}

impl AgentGuard {
    /// Close the agent now and report any failure, instead of on drop.
    pub async fn close(mut self) -> Result<()> {
        match self.agent.take() {
            Some(agent) => agent.close().await,
            None => Ok(()),
        }
    }
}

impl Drop for AgentGuard {
    fn drop(&mut self) {
        let Some(agent) = self.agent.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(error) = agent.close().await {
                        tracing::debug!(target: "cursor_sdk", %error, "closing the agent failed");
                    }
                });
            }
            Err(_) => {
                // No runtime left to spawn on — the program is shutting down,
                // and the bridge releases these resources when it exits.
                tracing::debug!(
                    target: "cursor_sdk",
                    agent_id = %agent.id(),
                    "no Tokio runtime at drop; the agent will be released when the bridge exits"
                );
            }
        }
    }
}
