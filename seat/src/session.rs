//! Durable at-most-once session (P5).
//!
//! `<session_dir>/session.jsonl` is an append-only log in unreal-agent's
//! codec shape (`{"type","data"}` records of kind
//! `session|item|operation`; items carry contiguous 1-based `seq`; a torn
//! trailing line without `\n` is uncommitted and dropped on load).
//! Only the shape is shared — the record bodies are the seat's own
//! format v1, not Go's session/item taxonomy.
//!
//! Write order is the crash invariant: the `result` item is always
//! appended **before** the terminal operation state, so a terminal state
//! without a result means corruption, never a normal crash.
//!
//! Re-invoking with the same `request_id` replays instead of re-sending:
//! terminal + result → return the stored result with no bridge contact;
//! awaiting + run identity → attach via `ObserveRun`; ready → send
//! normally. Awaiting *without* run identity is unresumable (the send
//! may have landed remotely) and fails closed. Retries with new work
//! must use a new `request_id` (new attempt); same-request re-invoke is
//! crash recovery only.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::protocol::SeatResult;

/// Session format version.
pub const SESSION_VERSION: u32 = 1;
/// Log file name inside the session dir.
pub const SESSION_FILE: &str = "session.jsonl";

/// Operation states. Transitions are validated by [`OpState::can_go`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    /// Session opened, nothing sent yet. Re-invoke may send.
    Ready,
    /// Stream opened. Re-invoke must attach, never re-send.
    Awaiting,
    /// A hard stop fired while awaiting.
    Canceling,
    /// Terminal: run succeeded.
    Completed,
    /// Terminal: run failed (or a pre-start failure closed the attempt).
    Failed,
    /// Terminal: run was cancelled.
    Canceled,
}

impl OpState {
    /// Whether the attempt is over (re-invoke replays, never sends).
    pub fn is_terminal(self) -> bool {
        matches!(self, OpState::Completed | OpState::Failed | OpState::Canceled)
    }

    /// Legal transitions. Mostly a strict subset of Go's remote-job
    /// table (no `Ready→Canceling`, no self-transitions), with two
    /// seat-specific extensions: `Canceling→Completed` (a cancel racing a
    /// natural finish reports the truth) and `Completed→Awaiting`
    /// (P6 self-check follow-up re-opens a finished attempt in-process;
    /// the new result overwrites, so reload still replays the latest).
    /// `Ready→Canceling` is unreachable (pre-run-id stops stay `pending`
    /// until `Awaiting`).
    fn can_go(self, next: OpState) -> bool {
        match (self, next) {
            (OpState::Ready, OpState::Awaiting) => true,
            (OpState::Ready, OpState::Failed) => true,
            (OpState::Awaiting, OpState::Canceling)
            | (OpState::Awaiting, OpState::Completed)
            | (OpState::Awaiting, OpState::Failed)
            | (OpState::Awaiting, OpState::Canceled) => true,
            (OpState::Canceling, OpState::Completed)
            | (OpState::Canceling, OpState::Failed)
            | (OpState::Canceling, OpState::Canceled) => true,
            (OpState::Completed, OpState::Awaiting) => true,
            _ => false,
        }
    }
}

/// Failures opening or updating the session log.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    #[error("session log I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("session log has invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session log is corrupt: {0}")]
    Corrupt(String),
    #[error("session log belongs to request `{0}`, not `{1}`")]
    ForeignRequest(String, String),
    #[error("illegal operation transition {0:?} -> {1:?}")]
    BadTransition(OpState, OpState),
    #[error("session cannot resume safely: {0}")]
    Unresumable(String),
}

/// What a loaded session asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// No usable history: send normally.
    Fresh,
    /// A run is live remotely: attach, never re-send.
    Resume {
        run_id: String,
        agent_id: String,
    },
    /// The attempt already finished: return the stored result.
    Replay,
}

/// Internal decision, including states [`SessionStore::open`] rejects.
#[derive(Debug)]
enum Decision {
    Fresh,
    Resume { run_id: String, agent_id: String },
    Replay,
    Broken(SessionError),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionData {
    v: u32,
    request_id: String,
    started_epoch_secs: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ItemBody {
    Run { run_id: String, agent_id: String },
    Result { result: SeatResult },
    Control { id: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemData {
    seq: u64,
    body: ItemBody,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OpData {
    id: String,
    state: OpState,
    #[serde(default)]
    run_id: Option<String>,
    updated_at_epoch_secs: u64,
}

/// Append-only session log with an in-memory image of its state.
pub struct SessionStore {
    path: PathBuf,
    request_id: String,
    next_seq: u64,
    op: OpState,
    run: Option<(String, String)>,
    result: Option<SeatResult>,
    seen_controls: Vec<String>,
}

impl SessionStore {
    /// Open (creating) `<dir>/session.jsonl` for `request_id`, recovering
    /// a torn tail, and decide fresh / resume / replay.
    pub fn open(dir: &Path, request_id: &str) -> Result<(Self, Opened), SessionError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(SESSION_FILE);
        let mut store = SessionStore {
            path,
            request_id: request_id.to_string(),
            next_seq: 1,
            op: OpState::Ready,
            run: None,
            result: None,
            seen_controls: Vec::new(),
        };
        if !store.path.exists() {
            store.append_session()?;
            return Ok((store, Opened::Fresh));
        }
        let raw = std::fs::read(&store.path)?;
        let committed_len = raw.iter().rposition(|byte| *byte == b'\n').map_or(0, |pos| pos + 1);
        if committed_len < raw.len() {
            // Torn tail: truncate it away (best-effort; the in-memory
            // image below still loads from the committed prefix).
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .open(&store.path)
                .and_then(|file| file.set_len(committed_len as u64));
        }
        if committed_len == 0 {
            // Nothing committed (empty or only a torn line): start over.
            store.append_session()?;
            return Ok((store, Opened::Fresh));
        }
        store.load(&raw[..committed_len])?;
        match store.internal_decision() {
            Decision::Fresh => Ok((store, Opened::Fresh)),
            Decision::Resume { run_id, agent_id } => Ok((store, Opened::Resume { run_id, agent_id })),
            Decision::Replay => Ok((store, Opened::Replay)),
            Decision::Broken(error) => Err(error),
        }
    }

    /// Recompute what this session asks of the caller.
    pub fn decision(&self) -> Opened {
        match self.internal_decision() {
            Decision::Fresh => Opened::Fresh,
            Decision::Resume { run_id, agent_id } => Opened::Resume { run_id, agent_id },
            Decision::Replay => Opened::Replay,
            // Post-open the log cannot regress into Broken (appends only
            // move forward), so collapse defensively to Fresh.
            Decision::Broken(_) => Opened::Fresh,
        }
    }

    fn internal_decision(&self) -> Decision {
        if self.op.is_terminal() {
            if self.result.is_none() {
                return Decision::Broken(SessionError::Corrupt(
                    "terminal operation without a result item".to_string(),
                ));
            }
            return Decision::Replay;
        }
        if self.op == OpState::Awaiting {
            if let Some((run_id, agent_id)) = self.run.clone() {
                if self.result.is_none() {
                    return Decision::Resume { run_id, agent_id };
                }
                // Re-opened by a self-check follow-up: the latest result
                // wins on reload.
                return Decision::Replay;
            } else {
                return Decision::Broken(SessionError::Unresumable(
                    "stream opened but no run identity was recorded".to_string(),
                ));
            }
        }
        Decision::Fresh
    }

    /// Control ids already delivered (for inbox dedup seeding).
    pub fn seen_control_ids(&self) -> &[String] {
        &self.seen_controls
    }

    pub fn state(&self) -> OpState {
        self.op
    }

    pub fn stored_result(&self) -> Option<&SeatResult> {
        self.result.as_ref()
    }

    pub fn resume_target(&self) -> Option<(String, String)> {
        self.run.clone()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record the run identity (first `run_started`).
    pub fn record_run(&mut self, run_id: &str, agent_id: &str) -> Result<(), SessionError> {
        self.append_item(ItemBody::Run {
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
        })?;
        self.run = Some((run_id.to_string(), agent_id.to_string()));
        Ok(())
    }

    /// Record a delivered control id (dedup seeding across restarts).
    pub fn record_control(&mut self, id: &str) -> Result<(), SessionError> {
        if self.seen_controls.iter().any(|seen| seen == id) {
            return Ok(());
        }
        self.append_item(ItemBody::Control { id: id.to_string() })?;
        self.seen_controls.push(id.to_string());
        Ok(())
    }

    /// Record the terminal result (always before the terminal state).
    pub fn record_result(&mut self, result: &SeatResult) -> Result<(), SessionError> {
        self.append_item(ItemBody::Result {
            result: result.clone(),
        })?;
        self.result = Some(result.clone());
        Ok(())
    }

    /// Transition the operation, rejecting illegal moves.
    pub fn set_state(&mut self, next: OpState) -> Result<(), SessionError> {
        if !self.op.can_go(next) {
            return Err(SessionError::BadTransition(self.op, next));
        }
        self.append_record(
            "operation",
            &OpData {
                id: "attempt".to_string(),
                state: next,
                run_id: self.run.as_ref().map(|(run_id, _)| run_id.clone()),
                updated_at_epoch_secs: epoch_secs(),
            },
        )?;
        self.op = next;
        Ok(())
    }

    fn append_session(&self) -> Result<(), SessionError> {
        self.append_record(
            "session",
            &SessionData {
                v: SESSION_VERSION,
                request_id: self.request_id.clone(),
                started_epoch_secs: epoch_secs(),
            },
        )
    }

    fn append_item(&mut self, body: ItemBody) -> Result<(), SessionError> {
        self.append_record(
            "item",
            &ItemData {
                seq: self.next_seq,
                body,
            },
        )?;
        self.next_seq += 1;
        Ok(())
    }

    fn append_record(&self, kind: &str, data: &impl serde::Serialize) -> Result<(), SessionError> {
        let data = serde_json::to_value(data)?;
        let mut line = serde_json::to_vec(&serde_json::json!({"type": kind, "data": data}))?;
        if line.contains(&b'\n') {
            return Err(SessionError::Corrupt(
                "encoded record contains a newline".to_string(),
            ));
        }
        line.push(b'\n');
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(&line)?;
        Ok(())
    }

    fn load(&mut self, committed: &[u8]) -> Result<(), SessionError> {
        let text =
            std::str::from_utf8(committed).map_err(|_| SessionError::Corrupt("log is not UTF-8".to_string()))?;
        let mut lines = text.lines();
        let first = lines.next().ok_or_else(|| SessionError::Corrupt("log is empty".to_string()))?;
        let session: SessionData = record_data(first, "session")?;
        if session.v != SESSION_VERSION {
            return Err(SessionError::Corrupt(format!(
                "unsupported session version {}",
                session.v
            )));
        }
        if session.request_id != self.request_id {
            return Err(SessionError::ForeignRequest(
                session.request_id,
                self.request_id.clone(),
            ));
        }
        for line in lines {
            let kind = record_kind(line)?;
            match kind.as_str() {
                "item" => {
                    let item: ItemData = record_data(line, "item")?;
                    if item.seq != self.next_seq {
                        return Err(SessionError::Corrupt(format!(
                            "item seq {}, want {}",
                            item.seq, self.next_seq
                        )));
                    }
                    self.next_seq += 1;
                    match item.body {
                        ItemBody::Run { run_id, agent_id } => {
                            self.run = Some((run_id, agent_id));
                        }
                        ItemBody::Result { result } => {
                            self.result = Some(result);
                        }
                        ItemBody::Control { id } => {
                            if !self.seen_controls.iter().any(|seen| seen == &id) {
                                self.seen_controls.push(id);
                            }
                        }
                    }
                }
                "operation" => {
                    let op: OpData = record_data(line, "operation")?;
                    if op.id != "attempt" {
                        return Err(SessionError::Corrupt(format!(
                            "unknown operation `{}`",
                            op.id
                        )));
                    }
                    if !self.op.can_go(op.state) {
                        return Err(SessionError::BadTransition(self.op, op.state));
                    }
                    self.op = op.state;
                    if let Some(run_id) = op.run_id {
                        if let Some((current, _)) = self.run.as_ref() {
                            if current != &run_id {
                                return Err(SessionError::Corrupt(
                                    "operation names a different run".to_string(),
                                ));
                            }
                        }
                    }
                }
                other => {
                    return Err(SessionError::Corrupt(format!(
                        "unsupported record type `{other}`"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn record_kind(line: &str) -> Result<String, SessionError> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    value
        .get("type")
        .and_then(|kind| kind.as_str())
        .map(str::to_string)
        .ok_or_else(|| SessionError::Corrupt("record has no type".to_string()))
}

fn record_data<T: serde::de::DeserializeOwned>(line: &str, kind: &str) -> Result<T, SessionError> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    if value.get("type").and_then(|type_| type_.as_str()) != Some(kind) {
        return Err(SessionError::Corrupt(format!("expected a {kind} record")));
    }
    let data = value.get("data").ok_or_else(|| SessionError::Corrupt("record has no data".to_string()))?;
    serde_json::from_value(data.clone()).map_err(SessionError::Json)
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Outcome;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seat-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn failed_result() -> SeatResult {
        SeatResult {
            outcome: Outcome::Failed,
            status: "error".into(),
            error_kind: Some("Unknown".into()),
            retryable: false,
            retry_after_ms: None,
            request_id: "pkt:1".into(),
            run_id: Some("run_1".into()),
            agent_id: Some("agent_1".into()),
            model: Some("composer-2.5".into()),
            text: "bad".into(),
            archive_path: None,
            wall_ms: 5,
            ttfe_ms: None,
            usage: None,
            attempts: 3,
            self_check: None,
            context_changes: vec![],
            resumed: false,
        }
    }

    #[test]
    fn fresh_open_writes_the_session_record() {
        let dir = tempdir("fresh");
        let (store, opened) = SessionStore::open(&dir, "pkt:1").unwrap();
        assert_eq!(opened, Opened::Fresh);
        assert_eq!(store.state(), OpState::Ready);
        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(raw.ends_with('\n'));
        assert_eq!(raw.lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_history_replays() {
        let dir = tempdir("replay");
        let (mut store, _) = SessionStore::open(&dir, "pkt:1").unwrap();
        store.set_state(OpState::Awaiting).unwrap();
        store.record_run("run_1", "agent_1").unwrap();
        store.record_control("c1").unwrap();
        store.record_result(&failed_result()).unwrap();
        store.set_state(OpState::Failed).unwrap();
        drop(store);

        let (store, opened) = SessionStore::open(&dir, "pkt:1").unwrap();
        assert_eq!(opened, Opened::Replay);
        assert_eq!(store.stored_result().unwrap().text, "bad");
        assert_eq!(store.seen_control_ids(), &["c1".to_string()]);
        assert_eq!(
            store.resume_target().unwrap(),
            ("run_1".to_string(), "agent_1".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn awaiting_run_resumes() {
        let dir = tempdir("resume");
        let (mut store, _) = SessionStore::open(&dir, "pkt:1").unwrap();
        store.set_state(OpState::Awaiting).unwrap();
        store.record_run("run_9", "agent_9").unwrap();
        drop(store);

        let (_, opened) = SessionStore::open(&dir, "pkt:1").unwrap();
        assert_eq!(
            opened,
            Opened::Resume {
                run_id: "run_9".into(),
                agent_id: "agent_9".into(),
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_is_dropped() {
        let dir = tempdir("torn");
        let (mut store, _) = SessionStore::open(&dir, "pkt:1").unwrap();
        store.set_state(OpState::Awaiting).unwrap();
        store.record_run("run_1", "agent_1").unwrap();
        drop(store);
        // Simulate a kill mid-line.
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(SESSION_FILE))
            .unwrap()
            .write_all(b"{\"type\":\"item\",\"data\":{\"seq\":99")
            .unwrap();

        let (_, opened) = SessionStore::open(&dir, "pkt:1").unwrap();
        assert_eq!(
            opened,
            Opened::Resume {
                run_id: "run_1".into(),
                agent_id: "agent_1".into(),
            }
        );
        // The torn tail was truncated away.
        let raw = std::fs::read_to_string(dir.join(SESSION_FILE)).unwrap();
        assert!(raw.ends_with('\n'));
        assert!(!raw.contains("seq\":99"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_request_and_seq_gaps_fail_closed() {
        let dir = tempdir("foreign");
        SessionStore::open(&dir, "pkt:1").unwrap();
        assert!(matches!(
            SessionStore::open(&dir, "pkt:2"),
            Err(SessionError::ForeignRequest(_, _))
        ));

        // Hand-corrupt the seq.
        let path = dir.join(SESSION_FILE);
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            raw + "{\"type\":\"item\",\"data\":{\"seq\":7,\"body\":{\"kind\":\"control\",\"id\":\"x\"}}}\n",
        )
        .unwrap();
        assert!(matches!(
            SessionStore::open(&dir, "pkt:1"),
            Err(SessionError::Corrupt(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let dir = tempdir("trans");
        let (mut store, _) = SessionStore::open(&dir, "pkt:1").unwrap();
        assert!(store.set_state(OpState::Completed).is_err());
        store.set_state(OpState::Awaiting).unwrap();
        store.set_state(OpState::Canceling).unwrap();
        assert!(store.set_state(OpState::Awaiting).is_err());
        store.set_state(OpState::Canceled).unwrap();
        assert!(store.set_state(OpState::Failed).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn awaiting_without_run_is_unresumable() {
        let dir = tempdir("norun");
        let (mut store, _) = SessionStore::open(&dir, "pkt:1").unwrap();
        store.set_state(OpState::Awaiting).unwrap();
        drop(store);
        assert!(matches!(
            SessionStore::open(&dir, "pkt:1"),
            Err(SessionError::Unresumable(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
