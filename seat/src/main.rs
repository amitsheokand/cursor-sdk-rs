//! `cursor-seat` binary: one packet attempt on stdin/stdout.
//!
//! Line 1 is the `SeatRequest`; later lines are control inputs parsed and
//! submitted by the reader task. [`run_seat`](cursor_seat::run::run_seat)
//! owns the attempt and every stdout event with a contiguous `seq`.
//!
//! Exit codes (for P7): 0 when the result is `ok`, 1 when a `result`
//! event was emitted with any other outcome or stdin was invalid, 2 when
//! no `result` could be produced at all. A reader fault after the result
//! is reported via the exit code only: the emitted `result` stands.
//! Contract: P7 closes stdin after reading the `result` event; the binary
//! waits for that EOF before exiting so no control is lost mid-run.

use std::process::ExitCode;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use cursor_seat::inbox::{parse_control_line, Inbox, InboxError};
use cursor_seat::protocol::SeatRequest;
use cursor_seat::run::run_seat;
use cursor_seat::session::SessionStore;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(ok) if ok => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(Fatal::NoResult(message)) => {
            eprintln!("cursor-seat: {message}");
            ExitCode::from(2)
        }
        Err(Fatal::Failed(message)) => {
            eprintln!("cursor-seat: {message}");
            ExitCode::from(1)
        }
    }
}

enum Fatal {
    Failed(String),
    NoResult(String),
}

async fn run() -> Result<bool, Fatal> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    let first = lines
        .next_line()
        .await
        .map_err(|e| Fatal::Failed(format!("failed to read stdin: {e}")))?
        .ok_or_else(|| Fatal::Failed("missing SeatRequest on stdin line 1".to_string()))?;
    if first.trim().is_empty() {
        return Err(Fatal::Failed(
            "missing SeatRequest on stdin line 1".to_string(),
        ));
    }
    let mut request: SeatRequest = serde_json::from_str(&first)
        .map_err(|e| Fatal::Failed(format!("invalid SeatRequest: {e}")))?;
    request.validate().map_err(Fatal::Failed)?;

    let client = cursor_sdk::Client::builder()
        .workspace(&request.cwd)
        .build();
    // Durable session first: it seeds inbox dedup across restarts, and a
    // broken log fails closed before any bridge contact.
    let session = match request.session_dir.as_deref() {
        Some(dir) => {
            let (store, _) = SessionStore::open(std::path::Path::new(dir), &request.request_id)
                .map_err(|e| Fatal::Failed(format!("invalid session: {e}")))?;
            Some(store)
        }
        None => None,
    };
    let seen: Vec<String> = session
        .as_ref()
        .map(|store| store.seen_control_ids().to_vec())
        .unwrap_or_default();
    let inbox =
        Inbox::new(&seen).map_err(|e| Fatal::Failed(format!("failed to open inbox: {e}")))?;
    let reader = inbox.handle();
    let reader_task = tokio::spawn(async move {
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| format!("failed to read stdin: {e}"))?
        {
            if line.trim().is_empty() {
                continue;
            }
            // Fail fast on the first invalid line. A Closed submit means
            // the run already ended and closed the inbox: controls arriving
            // after the result are moot, not failures.
            let input =
                parse_control_line(&line).map_err(|e| format!("invalid control input: {e}"))?;
            match reader.submit(input) {
                Ok(()) => {}
                Err(InboxError::Closed) => break,
                Err(e) => return Err(format!("failed to submit control input: {e}")),
            }
        }
        Ok::<(), String>(())
    });

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(event) = events_rx.recv().await {
            let mut bytes =
                serde_json::to_vec(&event).map_err(|e| format!("failed to encode event: {e}"))?;
            bytes.push(b'\n');
            stdout
                .write_all(&bytes)
                .await
                .map_err(|e| format!("failed to write event: {e}"))?;
        }
        stdout
            .flush()
            .await
            .map_err(|e| format!("flush failed: {e}"))?;
        Ok::<(), String>(())
    });

    let result = run_seat(&client, request, inbox, events_tx, session).await;
    stdout_task
        .await
        .map_err(|e| Fatal::NoResult(format!("stdout task panicked: {e}")))?
        .map_err(Fatal::NoResult)?;
    let reader_outcome = reader_task
        .await
        .map_err(|e| Fatal::NoResult(format!("reader task panicked: {e}")))?;

    if let Err(message) = reader_outcome {
        eprintln!("cursor-seat: {message}");
        return Ok(false);
    }
    Ok(result.outcome == cursor_seat::protocol::Outcome::Ok)
}
