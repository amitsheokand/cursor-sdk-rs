//! Typed stdin control inbox, ported from unreal-agent `harness/inbox`.
//!
//! - [`parse_control_line`] validates one stdin line: known keys only,
//!   mode/parameters cross-checks, `heartbeat` requires a reason,
//!   `settings` requires a valid `reasoning_effort`
//!   (`low|medium|high|xhigh|max`). Invalid states are unrepresentable
//!   past this boundary.
//! - [`Inbox`] deduplicates by `id` forever and delivers FIFO over a
//!   `tokio::sync::mpsc` queue. `submit` never blocks: intake is an
//!   unbounded channel, which is the faithful port of Go's run-loop
//!   queue (`SubmitDoesNotWaitForOutputConsumer`) and safe here because
//!   the producer is stdin-paced and the dedup set caps distinct ids.
//!   (Deliberate deviation from the bounded-channel default.)
//! - Shutdown is cooperative via channel close: dropping the consumer
//!   ends the task; [`Inbox::close`] drops both ends and waits for the
//!   task so no detached task outlives the inbox.

use std::collections::{HashSet, VecDeque};

use tokio::sync::mpsc;

use crate::protocol::{ControlInput, ControlMode};

/// Valid `reasoning_effort` values (unreal-agent `llm` package:
/// `low|medium|high|xhigh|max`).
pub const VALID_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Failures parsing or submitting control input.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InboxError {
    /// The line parsed but is not a valid control input.
    #[error("invalid control input: {0}")]
    Invalid(String),
    /// The line is not valid JSON.
    #[error("failed to parse control input: {0}")]
    Json(#[from] serde_json::Error),
    /// The inbox task has ended.
    #[error("inbox is closed")]
    Closed,
}

/// Parse and validate one stdin control line.
///
/// Unknown keys are rejected, `parameters` presence is checked against
/// the raw object (an explicit `null` counts as present, mirroring Go),
/// and mode/parameters cross-rules are enforced, so callers never hold
/// an invalid [`ControlInput`].
pub fn parse_control_line(line: &str) -> Result<ControlInput, InboxError> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    let object = value
        .as_object()
        .ok_or_else(|| InboxError::Invalid("control input must be a JSON object".to_string()))?;
    for key in object.keys() {
        match key.as_str() {
            "id" | "kind" | "mode" | "reason" | "parameters" => {}
            other => return Err(InboxError::Invalid(format!("unknown field `{other}`"))),
        }
    }
    let parameters_present = object.contains_key("parameters");
    let input: ControlInput = serde_json::from_value(value)?;
    check_semantics(&input, parameters_present)?;
    Ok(input)
}

/// Semantic checks shared by the parse path and direct `submit`.
///
/// Asymmetry note: `submit` infers presence from `is_some()`, so it cannot
/// distinguish an explicit JSON `null` (which `parse_control_line`
/// rejects for non-settings modes). The binary always parses first, so
/// this only affects programmatic callers.
fn check_semantics(input: &ControlInput, parameters_present: bool) -> Result<(), InboxError> {
    if input.id.is_empty() {
        return Err(InboxError::Invalid("input id is empty".to_string()));
    }
    match input.mode {
        ControlMode::Hard | ControlMode::WhenIdle => {
            if parameters_present {
                return Err(InboxError::Invalid(format!(
                    "control mode `{}` does not accept parameters",
                    mode_name(input.mode)
                )));
            }
        }
        ControlMode::Heartbeat => {
            if parameters_present {
                return Err(InboxError::Invalid(
                    "control mode `heartbeat` does not accept parameters".to_string(),
                ));
            }
            if input.reason.is_empty() {
                return Err(InboxError::Invalid("heartbeat reason is empty".to_string()));
            }
        }
        ControlMode::Settings => {
            if !parameters_present {
                return Err(InboxError::Invalid(
                    "control mode `settings` requires parameters".to_string(),
                ));
            }
            match input
                .parameters
                .as_ref()
                .and_then(|params| params.reasoning_effort.as_deref())
            {
                Some(effort) if VALID_EFFORTS.contains(&effort) => {}
                _ => {
                    return Err(InboxError::Invalid(
                        "settings requires parameters.reasoning_effort \
                         to be one of low|medium|high|xhigh|max"
                            .to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn mode_name(mode: ControlMode) -> &'static str {
    match mode {
        ControlMode::Hard => "hard",
        ControlMode::WhenIdle => "when_idle",
        ControlMode::Heartbeat => "heartbeat",
        ControlMode::Settings => "settings",
    }
}

/// Deduplicating FIFO queue for control inputs, mirroring Go's `Inbox`.
pub struct Inbox {
    submit_tx: mpsc::UnboundedSender<ControlInput>,
    output_rx: mpsc::UnboundedReceiver<ControlInput>,
    task: tokio::task::JoinHandle<()>,
}

/// Clonable submit handle: the stdin reader holds one while [`Inbox`]
/// drives the queue. Dropping every handle closes submissions.
#[derive(Debug, Clone)]
pub struct InboxHandle {
    submit_tx: mpsc::UnboundedSender<ControlInput>,
}

impl InboxHandle {
    /// Validate and enqueue. Never blocks; duplicates are accepted and
    /// dropped by the task.
    pub fn submit(&self, input: ControlInput) -> Result<(), InboxError> {
        check_semantics(&input, input.parameters.is_some())?;
        self.submit_tx.send(input).map_err(|_| InboxError::Closed)?;
        Ok(())
    }
}

impl Inbox {
    /// Create an inbox that already considers `seen_ids` delivered
    /// (durable resume across restarts, P5).
    pub fn new(seen_ids: &[String]) -> Result<Self, InboxError> {
        let mut seen = HashSet::with_capacity(seen_ids.len());
        for id in seen_ids {
            if id.is_empty() {
                return Err(InboxError::Invalid("seen input id is empty".to_string()));
            }
            seen.insert(id.clone());
        }
        let (submit_tx, submit_rx) = mpsc::unbounded_channel();
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(seen, submit_rx, output_tx));
        Ok(Self {
            submit_tx,
            output_rx,
            task,
        })
    }

    /// Validate and enqueue. Never blocks. Duplicates are accepted and
    /// dropped by the task, mirroring Go (where `Submit` succeeds and the
    /// run loop dedups).
    pub fn submit(&self, input: ControlInput) -> Result<(), InboxError> {
        self.handle().submit(input)
    }

    /// A clonable submit handle for the stdin reader task.
    pub fn handle(&self) -> InboxHandle {
        InboxHandle {
            submit_tx: self.submit_tx.clone(),
        }
    }

    /// Next deduplicated input in FIFO order, or `None` once the task has
    /// ended and the queue is flushed.
    pub async fn recv(&mut self) -> Option<ControlInput> {
        self.output_rx.recv().await
    }

    /// Non-blocking [`Inbox::recv`]: `Empty` when nothing is queued yet,
    /// `Disconnected` once the task has ended. Used to drain ready
    /// controls ahead of a fast stream.
    pub fn try_recv(&mut self) -> Result<ControlInput, mpsc::error::TryRecvError> {
        self.output_rx.try_recv()
    }

    /// End the task and wait for it. Queued inputs are discarded, mirroring
    /// Go's context-cancel stop. Exits promptly even while submit handles
    /// are still alive: the task watches for the consumer going away.
    pub async fn close(self) {
        drop(self.submit_tx);
        drop(self.output_rx);
        let _ = self.task.await;
    }
}

async fn run(
    mut seen: HashSet<String>,
    mut submit_rx: mpsc::UnboundedReceiver<ControlInput>,
    output_tx: mpsc::UnboundedSender<ControlInput>,
) {
    let mut queued: VecDeque<ControlInput> = VecDeque::new();
    let mut submissions_open = true;
    loop {
        while let Some(next) = queued.pop_front() {
            if output_tx.send(next).is_err() {
                return; // consumer gone
            }
        }
        if !submissions_open {
            return; // flushed and nothing more can arrive
        }
        // Either side closing ends the task: submissions drained above,
        // consumer loss discards the queue (mirrors Go's cancel stop).
        tokio::select! {
            received = submit_rx.recv() => match received {
                None => submissions_open = false,
                Some(input) => {
                    if seen.insert(input.id.clone()) {
                        queued.push_back(input);
                    }
                }
            },
            _ = output_tx.closed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ControlKind, SettingsParams};

    fn control(id: &str, mode: ControlMode, reason: &str, effort: Option<&str>) -> ControlInput {
        ControlInput {
            id: id.to_string(),
            kind: ControlKind::Control,
            mode,
            reason: reason.to_string(),
            parameters: effort.map(|effort| SettingsParams {
                reasoning_effort: Some(effort.to_string()),
            }),
        }
    }

    fn hard(id: &str) -> ControlInput {
        control(id, ControlMode::Hard, "stop now", None)
    }

    async fn recv_one(inbox: &mut Inbox) -> ControlInput {
        tokio::time::timeout(std::time::Duration::from_secs(1), inbox.recv())
            .await
            .expect("timed out waiting for input")
            .expect("output closed")
    }

    async fn assert_no_input(inbox: &mut Inbox) {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), inbox.recv())
                .await
                .is_err(),
            "unexpected input delivered"
        );
    }

    // ---- local_test.go mirrors ----

    #[tokio::test]
    async fn outputs_new_inputs() {
        let mut inbox = Inbox::new(&[]).unwrap();
        let input = hard("input-1");
        inbox.submit(input.clone()).unwrap();
        assert_eq!(recv_one(&mut inbox).await, input);
        inbox.close().await;
    }

    #[tokio::test]
    async fn outputs_every_control_mode() {
        let mut inbox = Inbox::new(&[]).unwrap();
        let modes = [
            control("hard", ControlMode::Hard, "stop now", None),
            control("idle", ControlMode::WhenIdle, "stop now", None),
            control("hb", ControlMode::Heartbeat, "waiting", None),
            control("set", ControlMode::Settings, "", Some("high")),
        ];
        for input in &modes {
            inbox.submit(input.clone()).unwrap();
            assert_eq!(recv_one(&mut inbox).await, *input);
        }
        inbox.close().await;
    }

    #[tokio::test]
    async fn deduplicates_input_id() {
        let mut inbox = Inbox::new(&[]).unwrap();
        let first = hard("same-input");
        inbox.submit(first.clone()).unwrap();
        assert_eq!(recv_one(&mut inbox).await, first);
        // Same id, different mode: accepted by submit, dropped by the task.
        inbox
            .submit(control(
                "same-input",
                ControlMode::WhenIdle,
                "different",
                None,
            ))
            .unwrap();
        let second = hard("next-input");
        inbox.submit(second.clone()).unwrap();
        assert_eq!(recv_one(&mut inbox).await, second);
        assert_no_input(&mut inbox).await;
        inbox.close().await;
    }

    #[tokio::test]
    async fn deduplicates_queued_input_id() {
        let mut inbox = Inbox::new(&[]).unwrap();
        inbox.submit(hard("same-input")).unwrap();
        inbox
            .submit(control("same-input", ControlMode::Hard, "dup", None))
            .unwrap();
        let got = recv_one(&mut inbox).await;
        assert_eq!(got.reason, "stop now");
        assert_no_input(&mut inbox).await;
        inbox.close().await;
    }

    #[tokio::test]
    async fn deduplicates_concurrent_submissions() {
        let inbox = Inbox::new(&[]).unwrap();
        let mut handles = Vec::new();
        for _ in 0..100 {
            let tx = inbox.submit_tx.clone();
            handles.push(tokio::spawn(async move {
                tx.send(hard("same-input")).map_err(|_| InboxError::Closed)
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        let mut inbox = inbox;
        assert_eq!(recv_one(&mut inbox).await.id, "same-input");
        assert_no_input(&mut inbox).await;
        inbox.close().await;
    }

    #[tokio::test]
    async fn keeps_different_ids_distinct() {
        let mut inbox = Inbox::new(&[]).unwrap();
        inbox.submit(hard("first")).unwrap();
        inbox.submit(hard("second")).unwrap();
        assert_eq!(recv_one(&mut inbox).await.id, "first");
        assert_eq!(recv_one(&mut inbox).await.id, "second");
        inbox.close().await;
    }

    #[tokio::test]
    async fn accepts_concurrent_submissions() {
        let inbox = Inbox::new(&[]).unwrap();
        for index in 0..100 {
            let id = format!("id-{index:03}");
            let tx = inbox.submit_tx.clone();
            tokio::spawn(async move {
                tx.send(hard(&id)).unwrap();
            })
            .await
            .unwrap();
        }
        let mut inbox = inbox;
        let mut ids = Vec::new();
        for _ in 0..100 {
            ids.push(recv_one(&mut inbox).await.id);
        }
        ids.sort();
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(id, &format!("id-{index:03}"));
        }
        inbox.close().await;
    }

    #[tokio::test]
    async fn submit_does_not_wait_for_output_consumer() {
        let mut inbox = Inbox::new(&[]).unwrap();
        for index in 0..100 {
            inbox.submit(hard(&format!("{index}"))).unwrap();
        }
        for index in 0..100 {
            assert_eq!(recv_one(&mut inbox).await.id, format!("{index}"));
        }
        inbox.close().await;
    }

    #[tokio::test]
    async fn discards_recovered_ids() {
        let mut inbox = Inbox::new(&["recovered".to_string(), "recovered".to_string()]).unwrap();
        // Accepted by submit, dropped before delivery.
        inbox.submit(hard("recovered")).unwrap();
        inbox.submit(hard("new")).unwrap();
        assert_eq!(recv_one(&mut inbox).await.id, "new");
        assert_no_input(&mut inbox).await;
        inbox.close().await;
    }

    #[tokio::test]
    async fn stops_with_queued_inputs() {
        let inbox = Inbox::new(&[]).unwrap();
        for index in 0..10 {
            inbox.submit(hard(&format!("{index}"))).unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), inbox.close())
            .await
            .expect("close hung with queued inputs");
    }

    #[tokio::test]
    async fn rejects_invalid_input() {
        let inbox = Inbox::new(&[]).unwrap();
        assert!(inbox
            .submit(control("", ControlMode::Hard, "r", None))
            .is_err());
        assert!(inbox
            .submit(control("hb", ControlMode::Heartbeat, "", None))
            .is_err());
        assert!(inbox
            .submit(control("set", ControlMode::Settings, "", None))
            .is_err());
        inbox.close().await;
    }

    #[test]
    fn rejects_invalid_seen_id() {
        assert!(Inbox::new(&["".to_string()]).is_err());
    }

    // ---- control_test.go mirrors ----

    #[tokio::test]
    async fn control_messages_round_trip() {
        for mode in [
            ControlMode::Hard,
            ControlMode::WhenIdle,
            ControlMode::Heartbeat,
        ] {
            let mut inbox = Inbox::new(&[]).unwrap();
            let want = control("stop", mode, "stop now", None);
            let line = serde_json::to_string(&want).unwrap();
            let got = parse_control_line(&line).unwrap();
            inbox.submit(got.clone()).unwrap();
            assert_eq!(recv_one(&mut inbox).await, want);
            inbox.close().await;
        }
    }

    #[tokio::test]
    async fn settings_controls_round_trip() {
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let mut inbox = Inbox::new(&[]).unwrap();
            let want = control("settings", ControlMode::Settings, "", Some(effort));
            let line = serde_json::to_string(&want).unwrap();
            let got = parse_control_line(&line).unwrap();
            inbox.submit(got.clone()).unwrap();
            assert_eq!(recv_one(&mut inbox).await, want);
            inbox.close().await;
        }
    }

    #[test]
    fn rejects_invalid_control_lines() {
        let bad = [
            "",
            "null",
            "{}",
            r#"{"id":"x","kind":"control","mode":"soft","reason":"r"}"#,
            r#"{"id":"x","kind":"control","mode":42,"reason":"r"}"#,
            r#"{"id":"x","kind":"control","mode":"hard","reason":"r""#,
            r#"{"id":"x","kind":"control","mode":"hard","reason":"r","extra":true}"#,
            r#"{"id":"x","kind":"control","mode":"heartbeat","reason":"waiting","extra":true}"#,
            r#"{"id":"x","kind":"control","mode":"heartbeat"}"#,
            r#"{"id":"x","kind":"control","mode":"heartbeat","reason":""}"#,
            r#"{"id":"x","kind":"control","mode":"heartbeat","reason":null}"#,
            r#"{"id":"x","kind":"control","mode":"settings"}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":null}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":[]}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":"high"}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":""}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":null}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":"default"}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":"turbo"}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":42}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"model":"m"}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"model":"m","reasoning_effort":"high"}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":"high","extra":true}}"#,
            r#"{"id":"x","kind":"control","mode":"settings","parameters":{"reasoning_effort":"high"},"extra":true}"#,
            r#"{"id":"x","kind":"control","mode":"hard","reason":"r","parameters":{"reasoning_effort":"high"}}"#,
            r#"{"id":"x","kind":"control","mode":"when_idle","reason":"r","parameters":{"reasoning_effort":"high"}}"#,
            r#"{"id":"x","kind":"control","mode":"heartbeat","reason":"w","parameters":{"reasoning_effort":"high"}}"#,
            r#"{"id":"x","kind":"control","mode":"hard","reason":"r","parameters":null}"#,
            r#"{"id":"","kind":"control","mode":"hard","reason":"r"}"#,
            r#"{"id":"x","kind":"external","mode":"hard","reason":"r"}"#,
        ];
        for line in bad {
            assert!(parse_control_line(line).is_err(), "accepted: {line}");
        }
    }

    #[test]
    fn control_wire_strings_match_protocol() {
        assert_eq!(
            serde_json::to_value(ControlMode::WhenIdle).unwrap(),
            serde_json::json!("when_idle")
        );
        assert_eq!(
            serde_json::to_value(ControlMode::Heartbeat).unwrap(),
            serde_json::json!("heartbeat")
        );
        assert_eq!(
            serde_json::to_value(ControlMode::Settings).unwrap(),
            serde_json::json!("settings")
        );
    }
}
