//! The streaming surface of a turn.
//!
//! A [`Run`] wraps the server stream returned by `Send` (or by `ObserveRun`)
//! and applies the rules from the bridge's `docs/streaming.md`:
//!
//! * A message with no envelope case is a keepalive. It is skipped silently,
//!   never surfaced and never treated as the end of the stream.
//! * Envelope cases and `SdkMessage` types this crate does not know are
//!   skipped too, so a newer bridge does not break an older program.
//! * The last non-empty `offset` is tracked for resume — and only offsets that
//!   came from `ObserveRun` are ever sent back as `after_offset`, because live
//!   `Send` offsets use a different numbering.
//! * A dropped stream does **not** cancel the run. [`Run::resume`] reconnects
//!   and [`Run::wait`] falls back to `WaitLiveRun`.

use serde_json::Value as JsonValue;

use crate::client::Client;
use crate::error::{Error, Result};
use crate::json::struct_to_json;
use crate::proto;
use crate::transport::ServerStream;
use crate::types::RunOutcome;

/// One conversation message from the run stream.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StreamMessage {
    /// The discriminator: `system`, `assistant`, `user`, `tool_call`,
    /// `thinking`, `status`, `task`, `usage`, and more over time.
    pub kind: String,
    /// The payload, shaped as the public `@cursor/sdk` documentation describes.
    pub payload: JsonValue,
}

impl StreamMessage {
    /// The assistant text this message carries, if it carries any.
    ///
    /// Tolerant of the several shapes the payload can take: a content-block
    /// array under `message.content` or `content`, or a plain `text` field.
    pub fn text(&self) -> Option<String> {
        let mut collected = String::new();
        for candidate in [
            self.payload.get("message").and_then(|m| m.get("content")),
            self.payload.get("content"),
        ]
        .into_iter()
        .flatten()
        {
            match candidate {
                JsonValue::String(text) => collected.push_str(text),
                JsonValue::Array(blocks) => {
                    for block in blocks {
                        if block.get("type").and_then(JsonValue::as_str) == Some("text") {
                            if let Some(text) = block.get("text").and_then(JsonValue::as_str) {
                                collected.push_str(text);
                            }
                        }
                    }
                }
                _ => {}
            }
            if !collected.is_empty() {
                return Some(collected);
            }
        }
        match self.payload.get("text") {
            Some(JsonValue::String(text)) if !text.is_empty() => Some(text.clone()),
            _ => None,
        }
    }

    /// Whether this is an assistant message.
    pub fn is_assistant(&self) -> bool {
        self.kind == "assistant"
    }

    fn string_field(&self, name: &str) -> Option<&str> {
        self.payload
            .get(name)
            .or_else(|| self.payload.get("message")?.get(name))
            .and_then(JsonValue::as_str)
    }

    /// The run this message belongs to, when the payload names one.
    pub fn run_id(&self) -> Option<&str> {
        self.string_field("run_id")
            .or_else(|| self.string_field("runId"))
    }

    /// The agent this message belongs to, when the payload names one.
    pub fn agent_id(&self) -> Option<&str> {
        self.string_field("agent_id")
            .or_else(|| self.string_field("agentId"))
    }
}

/// An event from a run stream.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RunEvent {
    /// A conversation message.
    Message(StreamMessage),
    /// A raw streaming delta. Only emitted when
    /// [`SendOptions::deltas`](crate::SendOptions::deltas) is on.
    Delta {
        /// The delta discriminator.
        kind: String,
        /// The delta payload.
        payload: JsonValue,
    },
    /// A completed conversation step. Only emitted when
    /// [`SendOptions::steps`](crate::SendOptions::steps) is on.
    Step {
        /// The step discriminator.
        kind: String,
        /// The step payload.
        payload: JsonValue,
    },
    /// The run reached a terminal state. The stream ends right after this.
    ///
    /// Boxed to keep [`RunEvent`] small: a terminal result arrives once, while
    /// [`RunEvent::Message`] arrives constantly.
    Completed(Box<RunOutcome>),
}

impl RunEvent {
    /// The assistant text this event carries, if any.
    pub fn text(&self) -> Option<String> {
        match self {
            RunEvent::Message(message) if message.is_assistant() => message.text(),
            _ => None,
        }
    }
}

/// Where the current stream's offsets came from.
///
/// Live `Send` streams interleave non-durable events into their numbering, so
/// an offset from one is not a valid `ObserveRun` resume point — passing one
/// can silently skip events.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OffsetSource {
    Live,
    Durable,
}

/// A turn in progress, or a durable run being replayed.
///
/// Consume it incrementally with [`Run::next_event`] / [`Run::next_text`], or
/// block for the outcome with [`Run::wait`].
#[derive(Debug)]
pub struct Run {
    client: Client,
    agent_id: String,
    run_id: Option<String>,
    stream: Option<ServerStream<proto::RunStreamMessage>>,
    last_offset: Option<String>,
    offset_source: OffsetSource,
    outcome: Option<RunOutcome>,
    last_status_message: Option<String>,
}

impl Run {
    pub(crate) fn live(
        client: Client,
        agent_id: String,
        stream: ServerStream<proto::RunStreamMessage>,
    ) -> Self {
        Self {
            client,
            agent_id,
            run_id: None,
            stream: Some(stream),
            last_offset: None,
            offset_source: OffsetSource::Live,
            outcome: None,
            last_status_message: None,
        }
    }

    pub(crate) fn durable(
        client: Client,
        agent_id: String,
        run_id: String,
        stream: ServerStream<proto::RunStreamMessage>,
    ) -> Self {
        Self {
            client,
            agent_id,
            run_id: Some(run_id),
            stream: Some(stream),
            last_offset: None,
            offset_source: OffsetSource::Durable,
            outcome: None,
            last_status_message: None,
        }
    }

    /// The run's id, once the stream has revealed it.
    ///
    /// The first `system` message carries it, so this is populated after the
    /// first event — or immediately for a run opened with
    /// [`Agent::observe`](crate::Agent::observe).
    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    /// The agent this run belongs to.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The last durable offset seen, for resuming a dropped stream.
    pub fn last_offset(&self) -> Option<&str> {
        self.last_offset.as_deref()
    }

    /// The terminal outcome, once observed.
    pub fn outcome(&self) -> Option<&RunOutcome> {
        self.outcome.as_ref()
    }

    /// The next event, or `None` when the stream is finished.
    ///
    /// Keepalives and unrecognized envelope cases are consumed internally.
    pub async fn next_event(&mut self) -> Option<Result<RunEvent>> {
        loop {
            let stream = self.stream.as_mut()?;
            let message = match stream.next().await {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    self.stream = None;
                    return Some(Err(error));
                }
                None => {
                    self.stream = None;
                    return None;
                }
            };

            // Keepalives carry no offset; they must not advance bookkeeping.
            if let Some(offset) = message.offset.filter(|value| !value.is_empty()) {
                self.last_offset = Some(offset);
            }

            match message.envelope {
                // No envelope case: a keepalive, or a case from a newer
                // contract. Either way, a no-op.
                None => continue,
                Some(proto::run_stream_message::Envelope::SdkMessage(payload)) => {
                    let message = StreamMessage {
                        kind: payload.r#type,
                        payload: struct_to_json(payload.message.as_ref()),
                    };
                    if self.run_id.is_none() {
                        self.run_id = message.run_id().map(str::to_string);
                    }
                    // When a run fails, the readable reason arrives here, not
                    // in the terminal result's error_code.
                    if message.kind == "status" {
                        if let Some(text) = message
                            .payload
                            .get("message")
                            .and_then(JsonValue::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            self.last_status_message = Some(text.to_string());
                        }
                    }
                    return Some(Ok(RunEvent::Message(message)));
                }
                Some(proto::run_stream_message::Envelope::InteractionUpdate(update)) => {
                    return Some(Ok(RunEvent::Delta {
                        kind: update.r#type,
                        payload: struct_to_json(update.update.as_ref()),
                    }));
                }
                Some(proto::run_stream_message::Envelope::Step(step)) => {
                    return Some(Ok(RunEvent::Step {
                        kind: step.r#type,
                        payload: struct_to_json(step.step.as_ref()),
                    }));
                }
                Some(proto::run_stream_message::Envelope::Result(result)) => {
                    let mut outcome = RunOutcome::from_stream_result(result);
                    outcome.failure_message = self.last_status_message.clone();
                    if self.run_id.is_none() && !outcome.run_id.is_empty() {
                        self.run_id = Some(outcome.run_id.clone());
                    }
                    self.outcome = Some(outcome.clone());
                    return Some(Ok(RunEvent::Completed(Box::new(outcome))));
                }
                Some(proto::run_stream_message::Envelope::Done(done)) => {
                    if self.run_id.is_none() && !done.run_id.is_empty() {
                        self.run_id = Some(done.run_id);
                    }
                    // `done` is the last message; the stream closes after it.
                    self.stream = None;
                    return None;
                }
            }
        }
    }

    /// The next chunk of assistant text, skipping everything else.
    ///
    /// ```no_run
    /// # async fn demo(run: &mut cursor_sdk::Run) -> cursor_sdk::Result<()> {
    /// while let Some(text) = run.next_text().await {
    ///     print!("{}", text?);
    /// }
    /// # Ok(()) }
    /// ```
    pub async fn next_text(&mut self) -> Option<Result<String>> {
        loop {
            match self.next_event().await? {
                Ok(event) => {
                    if let Some(text) = event.text() {
                        return Some(Ok(text));
                    }
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// Adapt the run to a [`futures_core::Stream`] of events.
    pub fn into_stream(self) -> impl futures_core::Stream<Item = Result<RunEvent>> + Send {
        futures_util::stream::unfold(self, |mut run| async move {
            run.next_event().await.map(|event| (event, run))
        })
    }

    /// Reconnect to the run's durable event log after a dropped stream.
    ///
    /// Per `docs/streaming.md`, a live `Send` offset is not a valid
    /// `ObserveRun` resume point, so a run that was streaming live replays from
    /// the beginning and the caller de-duplicates. A run already reading the
    /// durable log resumes exactly after its last offset.
    pub async fn resume(&mut self) -> Result<()> {
        let run_id = self.run_id.clone().ok_or_else(|| {
            Error::Config(
                "this run has no id yet, so there is nothing to resume; the id arrives with the \
                 first stream event"
                    .to_string(),
            )
        })?;
        let after = match self.offset_source {
            OffsetSource::Durable => self.last_offset.clone(),
            OffsetSource::Live => None,
        };
        let stream = self
            .client
            .observe_run_stream(&run_id, after.as_deref())
            .await?;
        self.stream = Some(stream);
        self.offset_source = OffsetSource::Durable;
        if after.is_none() {
            self.last_offset = None;
        }
        Ok(())
    }

    /// Drain the stream and return the run's outcome.
    ///
    /// If the stream drops before a terminal result, this falls back to
    /// `WaitLiveRun` — the run keeps executing regardless of the connection.
    pub async fn wait(mut self) -> Result<RunOutcome> {
        loop {
            match self.next_event().await {
                Some(Ok(_)) => continue,
                Some(Err(error)) => {
                    // A transport failure is recoverable: the run is still
                    // executing on the bridge.
                    tracing::debug!(
                        target: "cursor_sdk::run",
                        %error,
                        "the run stream failed; falling back to WaitLiveRun"
                    );
                    break;
                }
                None => break,
            }
        }

        if let Some(outcome) = self.outcome.take() {
            return Ok(outcome);
        }

        let run_id = self.run_id.clone().ok_or_else(|| {
            Error::transport(
                "the run stream ended before it reported a run id, so the outcome cannot be \
                 recovered",
            )
        })?;
        let mut outcome = self.client.wait_live_run(&run_id).await?;
        outcome.failure_message = self.last_status_message.clone();
        Ok(outcome)
    }

    /// Wait for the run and return its final assistant text.
    ///
    /// Fails when the run did not finish successfully, so a caller that only
    /// wants the answer does not silently get an empty string.
    pub async fn text(self) -> Result<String> {
        let outcome = self.wait().await?;
        match outcome.failure_reason() {
            Some(reason) => Err(Error::Transport(format!(
                "the run did not succeed: {reason}"
            ))),
            None => Ok(outcome.text),
        }
    }

    /// Ask the bridge to cancel the run.
    ///
    /// The stream still delivers a terminal result with status
    /// [`Cancelled`](crate::RunStatus::Cancelled), so keep consuming events
    /// afterwards if you want it.
    pub async fn cancel(&self) -> Result<()> {
        let run_id = self.run_id.as_deref().ok_or_else(|| {
            Error::Config(
                "this run has no id yet, so it cannot be cancelled; the id arrives with the first \
                 stream event"
                    .to_string(),
            )
        })?;
        self.client.cancel_run(run_id, Some(&self.agent_id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(kind: &str, payload: JsonValue) -> StreamMessage {
        StreamMessage {
            kind: kind.to_string(),
            payload,
        }
    }

    #[test]
    fn reads_text_from_content_blocks() {
        let message = message(
            "assistant",
            json!({"message": {"content": [
                {"type": "text", "text": "Hello, "},
                {"type": "tool_use", "name": "read"},
                {"type": "text", "text": "world."},
            ]}}),
        );
        assert_eq!(message.text().as_deref(), Some("Hello, world."));
    }

    #[test]
    fn reads_text_from_a_flat_payload() {
        assert_eq!(
            message("assistant", json!({"text": "plain"}))
                .text()
                .as_deref(),
            Some("plain")
        );
        assert_eq!(
            message("assistant", json!({"content": "string content"}))
                .text()
                .as_deref(),
            Some("string content")
        );
    }

    #[test]
    fn a_tool_call_message_has_no_text() {
        assert_eq!(
            message("tool_call", json!({"status": "running", "name": "grep"})).text(),
            None
        );
    }

    #[test]
    fn finds_ids_under_either_naming_convention() {
        let snake = message("system", json!({"run_id": "r1", "agent_id": "a1"}));
        assert_eq!(snake.run_id(), Some("r1"));
        assert_eq!(snake.agent_id(), Some("a1"));

        let camel = message("system", json!({"message": {"runId": "r2"}}));
        assert_eq!(camel.run_id(), Some("r2"));
    }

    #[test]
    fn only_assistant_messages_yield_event_text() {
        let assistant = RunEvent::Message(message("assistant", json!({"text": "hi"})));
        assert_eq!(assistant.text().as_deref(), Some("hi"));

        let user = RunEvent::Message(message("user", json!({"text": "hi"})));
        assert_eq!(user.text(), None, "echoed user text is not model output");

        let delta = RunEvent::Delta {
            kind: "text-delta".into(),
            payload: json!({"text": "hi"}),
        };
        assert_eq!(delta.text(), None);
    }
}
