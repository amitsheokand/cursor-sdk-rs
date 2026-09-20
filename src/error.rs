//! The error taxonomy (mirrors the bridge's `docs/errors.md`).
//!
//! Everything this crate returns is an [`Error`]. Failed RPCs decode the
//! `sdk.v1.SdkErrorDetails` Connect detail and expose its `sdk_error_code`
//! through [`ErrorKind`], so callers match on a variant instead of grepping a
//! message string. `request_id`, `retry_after`, and `rate_limit` are preserved
//! on the error value.

use std::fmt;
use std::time::Duration;

use base64::Engine;

use crate::proto::SdkErrorCode;

/// Convenience alias for fallible operations in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure this crate can produce.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Locating, spawning, handshaking with, or stopping the bridge failed.
    ///
    /// These are adapter-side launch problems and never carry
    /// `SdkErrorDetails`; the bridge's captured stderr is usually the
    /// explanation.
    #[error("{0}")]
    Bridge(#[from] BridgeError),

    /// The HTTP connection to the bridge failed, or a stream ended early.
    #[error("transport error: {0}")]
    Transport(String),

    /// The bridge answered an RPC with an error.
    ///
    /// Boxed so that [`Error`] stays small: it is the `Err` half of every
    /// `Result` in this crate, and `RpcError` carries seven optional fields.
    #[error(transparent)]
    Rpc(Box<RpcError>),

    /// A protobuf message from the bridge could not be decoded.
    #[error("failed to decode a {message} message: {source}")]
    Decode {
        /// The message type that failed to decode.
        message: &'static str,
        /// The underlying prost error.
        #[source]
        source: prost::DecodeError,
    },

    /// A local I/O operation failed (token file, artifact write, socket bind).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The caller supplied options the bridge cannot be asked for.
    ///
    /// Raised before anything reaches the wire — for example a local agent
    /// without a model, which `CreateAgent` requires.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// An operation exceeded its deadline on this side of the wire.
    #[error("{operation} timed out after {}s", .timeout.as_secs_f64())]
    Timeout {
        /// What was being waited on.
        operation: &'static str,
        /// The deadline that elapsed.
        timeout: Duration,
    },
}

impl From<RpcError> for Error {
    fn from(error: RpcError) -> Self {
        Error::Rpc(Box::new(error))
    }
}

impl Error {
    pub(crate) fn transport(context: impl fmt::Display) -> Self {
        Error::Transport(context.to_string())
    }

    pub(crate) fn decode(message: &'static str, source: prost::DecodeError) -> Self {
        Error::Decode { message, source }
    }

    /// The stable taxonomy class, when this is a failed RPC.
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            Error::Rpc(error) => Some(error.kind),
            _ => None,
        }
    }

    /// The full Cursor Cloud request ID, when the bridge reported one.
    ///
    /// Log it verbatim: Cursor support traces requests by this value, and a
    /// truncated ID is not traceable.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Error::Rpc(error) => error.request_id.as_deref(),
            _ => None,
        }
    }

    /// The backend's suggested wait before retrying, when known.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Rpc(error) => error.retry_after,
            _ => None,
        }
    }

    /// Whether a retry could plausibly succeed without the caller changing
    /// anything: rate limits, upstream hiccups, and transport drops.
    ///
    /// Validation and auth failures are never retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Rpc(error) => matches!(
                error.kind,
                ErrorKind::RateLimited | ErrorKind::Upstream | ErrorKind::Internal
            ),
            _ => false,
        }
    }

    /// Whether this is an authentication failure — either the bridge bearer
    /// token or the Cursor API key.
    pub fn is_auth(&self) -> bool {
        self.kind() == Some(ErrorKind::Unauthenticated)
    }

    /// Whether the referenced agent or run does not exist.
    pub fn is_not_found(&self) -> bool {
        self.kind() == Some(ErrorKind::NotFound)
    }
}

/// The class of a failed RPC.
///
/// Derived from `SdkErrorDetails.sdk_error_code` when the bridge attached one,
/// and from the Connect code otherwise. New `sdk_error_code` values from a
/// newer bridge fall through to [`ErrorKind::Unknown`] while still preserving
/// the raw code on [`RpcError::sdk_error_code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Credentials were missing or rejected.
    Unauthenticated,
    /// The account's plan or the caller's role forbids the operation.
    PermissionDenied,
    /// Unknown agent or run.
    NotFound,
    /// The request or the agent options failed validation.
    Validation,
    /// A rate or usage limit was hit; honor `retry_after`.
    RateLimited,
    /// The agent is already executing a run.
    AgentBusy,
    /// The operation is not valid in the resource's current state.
    InvalidState,
    /// An upstream model provider failed.
    Upstream,
    /// An unexpected bridge or backend failure.
    Internal,
    /// The client cancelled the operation.
    Cancelled,
    /// Unclassified, including codes newer than this crate.
    Unknown,
}

impl ErrorKind {
    fn from_sdk_code(code: SdkErrorCode) -> Option<Self> {
        use SdkErrorCode::*;
        Some(match code {
            Unspecified => return None,
            Unauthorized | ApiKeyNotFound => ErrorKind::Unauthenticated,
            PlanRequired | RoleForbidden | FeatureUnavailable | RepositoryAccess => {
                ErrorKind::PermissionDenied
            }
            AgentNotFound | RunNotFound => ErrorKind::NotFound,
            ValidationError | InvalidModel | InvalidBranchName | RepositoryRequired
            | PrResolutionFailed => ErrorKind::Validation,
            UsageLimitExceeded | RateLimitExceeded => ErrorKind::RateLimited,
            AgentBusy => ErrorKind::AgentBusy,
            AgentArchived | RunNotCancellable => ErrorKind::InvalidState,
            UpstreamError => ErrorKind::Upstream,
            InternalError => ErrorKind::Internal,
            ClientCancelled => ErrorKind::Cancelled,
        })
    }

    fn from_connect_code(code: &str) -> Self {
        match code {
            "unauthenticated" => ErrorKind::Unauthenticated,
            "permission_denied" => ErrorKind::PermissionDenied,
            "not_found" => ErrorKind::NotFound,
            "invalid_argument" | "failed_precondition" | "out_of_range" => ErrorKind::Validation,
            "resource_exhausted" => ErrorKind::RateLimited,
            "aborted" => ErrorKind::InvalidState,
            "unavailable" => ErrorKind::Upstream,
            "internal" | "data_loss" => ErrorKind::Internal,
            "canceled" | "cancelled" => ErrorKind::Cancelled,
            _ => ErrorKind::Unknown,
        }
    }
}

/// Rate-limit metadata from a failed RPC, when the backend reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests permitted in the current window.
    pub limit: Option<u64>,
    /// Requests still available in the current window.
    pub remaining: Option<u64>,
    /// Unix epoch seconds at which the window resets.
    pub reset_epoch_seconds: Option<u64>,
}

/// A failed RPC: the Connect code plus the bridge's structured detail.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RpcError {
    /// The stable class to branch on.
    pub kind: ErrorKind,
    /// The Connect/gRPC code, lowercase (`"unauthenticated"`, `"not_found"`, …).
    pub connect_code: String,
    /// `SdkErrorDetails.sdk_error_code` as an integer, or `0` when no detail
    /// was attached. Kept raw so codes newer than this crate stay readable.
    pub sdk_error_code: i32,
    /// Human-readable summary.
    pub message: String,
    /// Full Cursor Cloud request ID. Never truncate it when logging.
    pub request_id: Option<String>,
    /// Docs or remediation link.
    pub help_url: Option<String>,
    /// Upstream provider name, when the failure originated outside Cursor.
    pub provider: Option<String>,
    /// Suggested wait before retrying.
    pub retry_after: Option<Duration>,
    /// Rate-limit window metadata.
    pub rate_limit: Option<RateLimit>,
    /// The RPC that failed, as `Service/Method`.
    pub rpc: String,
}

impl RpcError {
    /// The `sdk_error_code` as a named enum, when this crate knows the value.
    pub fn sdk_error_code_name(&self) -> Option<&'static str> {
        SdkErrorCode::try_from(self.sdk_error_code)
            .ok()
            .map(|code| code.as_str_name())
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {} ({})",
            self.connect_code, self.message, self.rpc
        )?;
        if let Some(name) = self.sdk_error_code_name() {
            if self.sdk_error_code != 0 {
                write!(formatter, " [{name}]")?;
            }
        } else if self.sdk_error_code != 0 {
            write!(formatter, " [sdk_error_code={}]", self.sdk_error_code)?;
        }
        if let Some(request_id) = &self.request_id {
            write!(formatter, " (request_id={request_id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

/// A failure in the bridge process lifecycle.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BridgeError {
    /// No `cursor-sdk-bridge` executable could be found.
    #[error(
        "cursor-sdk-bridge executable not found: {0}.\n\
         Install one with:  cargo install cursor-sdk-bridge-fetch && cursor-sdk-bridge-fetch\n\
         Or: `pip install cursor-sdk` (puts it on PATH), set CURSOR_SDK_BRIDGE_BIN, pass \
         Client::builder().bridge_binary(path), or attach to a running bridge with \
         Client::builder().endpoint(url, token)"
    )]
    NotFound(String),

    /// The bridge archive's `manifest.json` says this binary cannot work here.
    #[error("the bridge at {path} cannot be used: {problem}")]
    Manifest {
        /// The executable whose manifest was checked.
        path: String,
        /// What the manifest said that ruled it out.
        problem: String,
    },

    /// The process could not be spawned.
    #[error("failed to spawn {path}: {source}")]
    Spawn {
        /// The executable that could not be started.
        path: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },

    /// The bridge exited before printing its ready line.
    #[error("bridge exited before it was ready ({status}). stderr:\n{stderr}")]
    ExitedBeforeReady {
        /// How the process ended.
        status: String,
        /// Everything the bridge wrote to stderr, which normally explains why.
        stderr: String,
    },

    /// The ready line did not arrive within the startup timeout.
    #[error("bridge did not become ready within {}s. stderr:\n{stderr}", .timeout.as_secs_f64())]
    StartupTimeout {
        /// The deadline that elapsed.
        timeout: Duration,
        /// Everything the bridge wrote to stderr so far.
        stderr: String,
    },

    /// The ready line was present but unusable.
    ///
    /// The raw line is deliberately not included: older bridges inline the
    /// bearer token in it.
    #[error("bridge handshake failed: {0}")]
    Handshake(String),

    /// The bearer token could not be read from `authTokenFile`.
    #[error("failed to read the bridge auth token from {path}: {source}")]
    AuthToken {
        /// The token file path from the ready line.
        path: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Decode a Connect error body into an [`RpcError`].
///
/// A Connect unary error is a non-200 response whose JSON body is
/// `{"code", "message", "details": [...]}`; a streaming error arrives in the
/// end-of-stream frame under the same shape. Detail values are unpadded
/// base64 of the serialized `google.protobuf.Any` payload.
pub(crate) fn rpc_error_from_connect(body: &[u8], http_status: Option<u16>, rpc: &str) -> RpcError {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct ConnectError {
        code: String,
        message: String,
        details: Vec<ConnectDetail>,
    }
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct ConnectDetail {
        #[serde(rename = "type")]
        type_url: String,
        value: String,
    }

    let parsed: ConnectError = serde_json::from_slice(body).unwrap_or_else(|_| ConnectError {
        code: code_from_http_status(http_status).to_string(),
        message: String::from_utf8_lossy(body).into_owned(),
        details: Vec::new(),
    });

    let connect_code = if parsed.code.is_empty() {
        code_from_http_status(http_status).to_string()
    } else {
        parsed.code
    };

    let details = parsed
        .details
        .iter()
        .find(|detail| detail.type_url.ends_with("sdk.v1.SdkErrorDetails"))
        .and_then(|detail| decode_base64(&detail.value))
        .and_then(|bytes| {
            <crate::proto::SdkErrorDetails as prost::Message>::decode(&bytes[..]).ok()
        });

    let mut error = RpcError {
        kind: ErrorKind::from_connect_code(&connect_code),
        connect_code,
        sdk_error_code: 0,
        message: parsed.message,
        request_id: None,
        help_url: None,
        provider: None,
        retry_after: None,
        rate_limit: None,
        rpc: rpc.to_string(),
    };

    if let Some(details) = details {
        error.sdk_error_code = details.sdk_error_code;
        // sdk_error_code is the stable taxonomy, so it wins over the Connect
        // code whenever the bridge classified the failure.
        if let Some(kind) = SdkErrorCode::try_from(details.sdk_error_code)
            .ok()
            .and_then(ErrorKind::from_sdk_code)
        {
            error.kind = kind;
        }
        if !details.message.is_empty() {
            error.message = details.message;
        }
        error.request_id = details.request_id.filter(|value| !value.is_empty());
        error.help_url = details.help_url.filter(|value| !value.is_empty());
        error.provider = details.provider.filter(|value| !value.is_empty());
        error.retry_after = details.retry_after.and_then(|duration| {
            u64::try_from(duration.seconds)
                .ok()
                .map(|seconds| Duration::new(seconds, duration.nanos.clamp(0, 999_999_999) as u32))
        });
        error.rate_limit = details.rate_limit.map(|info| RateLimit {
            limit: info.limit,
            remaining: info.remaining,
            reset_epoch_seconds: info.reset_epoch_seconds,
        });
    }

    if error.message.is_empty() {
        error.message = "the bridge reported an error with no message".to_string();
    }
    error
}

fn code_from_http_status(status: Option<u16>) -> &'static str {
    match status {
        Some(400) => "invalid_argument",
        Some(401) => "unauthenticated",
        Some(403) => "permission_denied",
        Some(404) => "not_found",
        Some(408) => "deadline_exceeded",
        Some(429) => "resource_exhausted",
        Some(500) => "internal",
        Some(503) => "unavailable",
        _ => "unknown",
    }
}

/// Connect specifies unpadded base64; real encoders differ on the alphabet, so
/// try both and tolerate padding either way.
fn decode_base64(value: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    let unpadded = value.trim_end_matches('=');
    STANDARD_NO_PAD
        .decode(unpadded)
        .or_else(|_| URL_SAFE_NO_PAD.decode(unpadded))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn encode_details(details: crate::proto::SdkErrorDetails) -> String {
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(details.encode_to_vec())
    }

    #[test]
    fn sdk_error_code_wins_over_connect_code() {
        let details = crate::proto::SdkErrorDetails {
            request_id: Some("req_abc123".into()),
            sdk_error_code: SdkErrorCode::AgentBusy as i32,
            message: "agent is already running".into(),
            retry_after: Some(prost_types::Duration {
                seconds: 5,
                nanos: 0,
            }),
            ..Default::default()
        };
        let body = serde_json::json!({
            "code": "internal",
            "message": "generic",
            "details": [{"type": "type.googleapis.com/sdk.v1.SdkErrorDetails",
                         "value": encode_details(details)}],
        });
        let error = rpc_error_from_connect(
            body.to_string().as_bytes(),
            Some(500),
            "SdkAgentService/Send",
        );

        assert_eq!(error.kind, ErrorKind::AgentBusy);
        assert_eq!(error.message, "agent is already running");
        assert_eq!(error.request_id.as_deref(), Some("req_abc123"));
        assert_eq!(error.retry_after, Some(Duration::from_secs(5)));
        assert_eq!(error.connect_code, "internal");
    }

    #[test]
    fn bare_unauthenticated_has_no_detail() {
        let body = br#"{"code":"unauthenticated","message":"Unauthorized"}"#;
        let error = rpc_error_from_connect(body, Some(401), "SdkBridgeControlService/Ping");
        assert_eq!(error.kind, ErrorKind::Unauthenticated);
        assert_eq!(error.sdk_error_code, 0);
        assert!(Error::from(error).is_auth());
    }

    #[test]
    fn unknown_sdk_code_stays_readable() {
        let details = crate::proto::SdkErrorDetails {
            sdk_error_code: 9999,
            message: "from a newer bridge".into(),
            ..Default::default()
        };
        let body = serde_json::json!({
            "code": "unknown",
            "message": "",
            "details": [{"type": "sdk.v1.SdkErrorDetails", "value": encode_details(details)}],
        });
        let error = rpc_error_from_connect(body.to_string().as_bytes(), None, "X/Y");
        assert_eq!(error.kind, ErrorKind::Unknown);
        assert_eq!(error.sdk_error_code, 9999);
        assert_eq!(error.sdk_error_code_name(), None);
    }

    #[test]
    fn non_json_body_falls_back_to_http_status() {
        let error = rpc_error_from_connect(b"<html>502</html>", Some(503), "X/Y");
        assert_eq!(error.connect_code, "unavailable");
        assert_eq!(error.kind, ErrorKind::Upstream);
    }
}
