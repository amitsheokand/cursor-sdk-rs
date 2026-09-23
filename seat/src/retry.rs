//! Pre-start error classification and retry (P3).
//!
//! Two splits matter, both frozen by `PROTOCOL.md`:
//!
//! - [`classify`] maps a [`cursor_sdk::Error`] raised **before** a run
//!   started onto the seat outcome table. After a run starts the seat
//!   resumes the stream instead (P4) — it never re-sends.
//! - In-seat retry ([`should_retry_in_seat`]) covers only transient wire
//!   failures (`Upstream | Internal | transport | Timeout`).
//!   `RateLimited | AgentBusy` become `busy` with `retry_after_ms` for
//!   the drain's `YieldBusy` wait instead: retrying them in-seat would
//!   double-wait (once here, once in the drain).
//!
//! Backoff is `2s << attempt` capped at 30s over [`MAX_ATTEMPTS`]
//! attempts, with deterministic ±20% jitter (see [`retry_delay`]).

use std::time::Duration;

use cursor_sdk::{Error, ErrorKind, RunStatus};

use crate::protocol::Outcome;

/// Total start attempts before a transient failure is terminal.
pub const MAX_ATTEMPTS: u32 = 5;
/// First backoff step.
pub const BASE_DELAY: Duration = Duration::from_secs(2);
/// Backoff cap; the exponential part never exceeds this.
pub const MAX_DELAY: Duration = Duration::from_secs(30);
/// Jitter band around each backoff step.
pub const JITTER_FRACTION: f64 = 0.2;

/// A classified pre-start failure, ready to become a `result` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatFailure {
    pub outcome: Outcome,
    pub error_kind: Option<String>,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
    /// Full Cursor Cloud request id, cloned verbatim — never truncated.
    pub request_id: Option<String>,
}

/// Map a pre-start error onto the outcome table.
///
/// Mapping: `Unauthenticated | PermissionDenied | Validation → bounced`
/// (a `Validation` bounce carries the kind as the reason and the drain
/// calls `next_model`); `RateLimited | AgentBusy → busy` with
/// `retry_after_ms`; `Upstream | Internal | transport | Timeout →`
/// `startup_error` + retryable (after [`MAX_ATTEMPTS`]);
/// `NotFound | InvalidState | Cancelled → stale`;
/// `Unknown → failed` keeping the raw code for P6 Jev triage.
/// Launch/config/decode/io failures never reached the backend, so they
/// are `startup_error` (`Config` is `bounced`: the caller must change
/// something, like a `Validation`).
pub fn classify(error: &Error) -> SeatFailure {
    let request_id = error.request_id().map(str::to_string);
    let retry_after_ms = error
        .retry_after()
        .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
    let failure = |outcome, error_kind: Option<String>, retryable| SeatFailure {
        outcome,
        error_kind,
        retry_after_ms,
        request_id: request_id.clone(),
        retryable,
    };
    match error {
        Error::Rpc(rpc) => {
            let kind = Some(format!("{:?}", rpc.kind));
            match rpc.kind {
                ErrorKind::Unauthenticated
                | ErrorKind::PermissionDenied
                | ErrorKind::Validation => failure(Outcome::Bounced, kind, false),
                ErrorKind::RateLimited | ErrorKind::AgentBusy => {
                    failure(Outcome::Busy, kind, true)
                }
                ErrorKind::Upstream | ErrorKind::Internal => {
                    failure(Outcome::StartupError, kind, true)
                }
                ErrorKind::NotFound | ErrorKind::InvalidState | ErrorKind::Cancelled => {
                    failure(Outcome::Stale, kind, false)
                }
                // Future kinds behave like Unknown: fail closed, keep the
                // code for Jev triage.
                _ => failure(Outcome::Failed, kind, false),
            }
        }
        Error::Transport(_) | Error::Timeout { .. } => {
            failure(Outcome::StartupError, None, true)
        }
        Error::Config(_) => failure(Outcome::Bounced, None, false),
        Error::Bridge(_) | Error::Decode { .. } | Error::Io(_) => {
            failure(Outcome::StartupError, None, false)
        }
        // Future error shapes: fail closed as a non-retryable startup error.
        _ => failure(Outcome::StartupError, None, false),
    }
}

/// Whether a pre-start error is worth another start attempt in-seat.
///
/// Deliberately narrower than [`Error::is_retryable`]: `RateLimited`
/// reports retryable but is handled via `busy` + drain-side wait, not an
/// in-seat sleep; conversely `Timeout` reports non-retryable upstream but
/// is transient wire trouble, so the seat retries it. Likewise the
/// result's `retryable` flag means "the drain should wait/retry", which
/// is why `busy` sets it even though `is_retryable(AgentBusy)` is false.
pub fn should_retry_in_seat(error: &Error) -> bool {
    matches!(error, Error::Transport(_) | Error::Timeout { .. })
        || matches!(
            error.kind(),
            Some(ErrorKind::Upstream | ErrorKind::Internal)
        )
}

/// Map a terminal run status onto the outcome table; `None` while the run
/// is still setting up or executing.
///
/// `Cancelled` (even self-inflicted via the inbox) and `Unknown` are
/// `failed`: the table has no cancelled bucket, and fail-closed beats a
/// silent pass. `Expired` outlived its deadline with no result, so a
/// fresh attempt may succeed: `stale`, which the drain retries without
/// spending a steer. (P4's `limits.timeout_s` backstops a run that never
/// reaches a terminal status.)
pub fn classify_run_status(status: RunStatus) -> Option<Outcome> {
    match status {
        RunStatus::Finished => Some(Outcome::Ok),
        RunStatus::Error => Some(Outcome::Failed),
        RunStatus::Cancelled => Some(Outcome::Failed),
        RunStatus::Expired => Some(Outcome::Stale),
        RunStatus::Creating | RunStatus::Running => None,
        _ => Some(Outcome::Failed),
    }
}

/// Backoff policy for pre-start retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: MAX_ATTEMPTS,
            base: BASE_DELAY,
            cap: MAX_DELAY,
        }
    }
}

/// Delay before retrying after failed start attempt `attempt` (0-based).
///
/// Exponential `base << attempt` capped at `cap`, ±20% jitter, and a
/// server `retry_after` wins whenever it exceeds the computed delay (the
/// cap binds only the exponential part).
pub fn retry_delay(
    policy: RetryPolicy,
    key: &str,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Duration {
    let shift = attempt.min(10);
    let backoff = policy
        .base
        .checked_mul(1 << shift)
        .unwrap_or(policy.cap)
        .min(policy.cap);
    let jittered = backoff.mul_f64(1.0 + jitter_fraction(key, attempt));
    match retry_after {
        Some(after) if after > jittered => after,
        _ => jittered,
    }
}

/// Deterministic ±20% jitter from `(key, attempt)`.
///
/// Uses a fixed-key hasher so delays are stable across restarts (replay
/// friendly). Randomness would only matter for desynchronizing many
/// clients; a seat is one client, so reproducibility wins.
fn jitter_fraction(key: &str, attempt: u32) -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    attempt.hash(&mut hasher);
    let unit = hasher.finish() as f64 / u64::MAX as f64;
    (unit * 2.0 - 1.0) * JITTER_FRACTION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_bands_match_two_four_eight_sixteen() {
        let policy = RetryPolicy::default();
        // attempt i (0-based) centers on 2<<i seconds within ±20%.
        for (attempt, center_secs) in [2, 4, 8, 16].iter().enumerate() {
            let delay = retry_delay(policy, "pkt:1", attempt as u32, None);
            let band = *center_secs as f64 * 0.8..=*center_secs as f64 * 1.2;
            assert!(
                band.contains(&(delay.as_secs_f64())),
                "attempt {attempt}: {delay:?} outside {band:?}"
            );
        }
    }

    #[test]
    fn backoff_caps_and_honors_retry_after() {
        let policy = RetryPolicy {
            max_attempts: 9,
            base: Duration::from_secs(2),
            cap: Duration::from_secs(30),
        };
        // 2<<7 = 256s capped to 30s, ±20% stays under 36s.
        let delay = retry_delay(policy, "pkt:1", 7, None);
        assert!(delay <= Duration::from_secs(36), "{delay:?}");
        // A longer server retry_after always wins, even above the cap.
        let honored = retry_delay(policy, "pkt:1", 0, Some(Duration::from_secs(120)));
        assert_eq!(honored, Duration::from_secs(120));
        // A shorter one does not shorten the backoff.
        let kept = retry_delay(policy, "pkt:1", 2, Some(Duration::from_millis(100)));
        assert!(kept >= Duration::from_secs(6), "{kept:?}");
    }

    #[test]
    fn jitter_is_deterministic_but_keyed() {
        let policy = RetryPolicy::default();
        let first = retry_delay(policy, "pkt:1", 2, None);
        assert_eq!(first, retry_delay(policy, "pkt:1", 2, None));
        // A second key stays inside the same band (no assertion that the
        // draws differ: DefaultHasher's algorithm is toolchain-pinned, so
        // a difference assertion could flip on upgrade).
        let second = retry_delay(policy, "pkt:2", 2, None);
        assert!((6.4..=9.6).contains(&second.as_secs_f64()));
    }

    #[test]
    fn run_status_table() {
        use cursor_sdk::RunStatus as Status;
        assert_eq!(classify_run_status(Status::Finished), Some(Outcome::Ok));
        assert_eq!(classify_run_status(Status::Error), Some(Outcome::Failed));
        assert_eq!(
            classify_run_status(Status::Cancelled),
            Some(Outcome::Failed)
        );
        assert_eq!(classify_run_status(Status::Expired), Some(Outcome::Stale));
        assert_eq!(classify_run_status(Status::Creating), None);
        assert_eq!(classify_run_status(Status::Running), None);
        assert_eq!(classify_run_status(Status::Unknown), Some(Outcome::Failed));
    }
}
