//! Custom agent stores: durable local agent state owned by this process.
//!
//! By default the bridge persists local agent state itself. Setting
//! [`LocalStore::Custom`](crate::LocalStore) hands the whole store to the
//! adapter: the bridge forwards every operation over a single `CallStore` RPC,
//! which this module routes to an [`AgentStore`] implementation.
//!
//! Three rules decide whether a store works, from the bridge's
//! `docs/services.md`:
//!
//! 1. **Return the bare record.** `create` and `update` *inputs* wrap the
//!    record under a singular key (`{"agent": {…}}`), but the *output* must be
//!    the record itself. Echoing the wrapper back causes opaque internal
//!    errors in the bridge.
//! 2. **`None` means null.** A `get` miss and a `delete` return no output.
//! 3. **Checkpoint blobs are base64 strings**, not byte arrays.
//!
//! The store endpoint can only be configured when the bridge launches, because
//! agents may load state before any RPC arrives — so a custom store must be set
//! on [`ClientBuilder`](crate::ClientBuilder), not after the fact.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value as JsonValue};

use super::{BoxFuture, HandlerResult};

/// Which part of the local agent store an operation addresses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Substore {
    /// Agent records.
    Agents,
    /// Run records.
    Runs,
    /// The durable run event log.
    RunEvents,
    /// Checkpoint blobs.
    Checkpoints,
    /// A substore newer than this crate.
    Other(String),
}

impl Substore {
    fn parse(value: &str) -> Self {
        match value {
            "agents" => Substore::Agents,
            "runs" => Substore::Runs,
            "runEvents" => Substore::RunEvents,
            "checkpoints" => Substore::Checkpoints,
            other => Substore::Other(other.to_string()),
        }
    }

    /// The wire name.
    pub fn as_str(&self) -> &str {
        match self {
            Substore::Agents => "agents",
            Substore::Runs => "runs",
            Substore::RunEvents => "runEvents",
            Substore::Checkpoints => "checkpoints",
            Substore::Other(value) => value,
        }
    }
}

/// The operation being performed on a substore.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StoreMethod {
    /// Fetch one record. Returning `None` is a miss.
    Get,
    /// Insert a record. The input wraps it under a singular key.
    Create,
    /// Modify a record. The input wraps it under a singular key.
    Update,
    /// Remove a record. Returns no output.
    Delete,
    /// Enumerate records.
    List,
    /// Append to the run event log. `runEvents` only.
    Append,
    /// A method newer than this crate.
    Other(String),
}

impl StoreMethod {
    fn parse(value: &str) -> Self {
        match value {
            "get" => StoreMethod::Get,
            "create" => StoreMethod::Create,
            "update" => StoreMethod::Update,
            "delete" => StoreMethod::Delete,
            "list" => StoreMethod::List,
            "append" => StoreMethod::Append,
            other => StoreMethod::Other(other.to_string()),
        }
    }

    /// The wire name.
    pub fn as_str(&self) -> &str {
        match self {
            StoreMethod::Get => "get",
            StoreMethod::Create => "create",
            StoreMethod::Update => "update",
            StoreMethod::Delete => "delete",
            StoreMethod::List => "list",
            StoreMethod::Append => "append",
            StoreMethod::Other(value) => value,
        }
    }
}

/// One store operation forwarded by the bridge.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StoreRequest {
    /// Which substore.
    pub substore: Substore,
    /// Which operation.
    pub method: StoreMethod,
    /// The operation input, always a JSON object.
    pub input: JsonValue,
}

impl StoreRequest {
    pub(crate) fn new(substore: &str, method: &str, input: JsonValue) -> Self {
        Self {
            substore: Substore::parse(substore),
            method: StoreMethod::parse(method),
            input,
        }
    }

    /// A field of the input object.
    pub fn field(&self, name: &str) -> Option<&JsonValue> {
        self.input.get(name)
    }

    /// A string field of the input object.
    pub fn string_field(&self, name: &str) -> Option<&str> {
        self.input.get(name)?.as_str()
    }

    /// Unwrap the record a `create`/`update` input carries.
    ///
    /// The bridge wraps it under a singular key derived from the substore
    /// (`agents` → `agent`). Falls back to the whole input when no wrapper is
    /// present, so an unexpected shape still round-trips.
    pub fn record(&self) -> &JsonValue {
        let singular = match &self.substore {
            Substore::Agents => "agent",
            Substore::Runs => "run",
            Substore::RunEvents => "runEvent",
            Substore::Checkpoints => "checkpoint",
            Substore::Other(_) => return &self.input,
        };
        self.input.get(singular).unwrap_or(&self.input)
    }
}

/// A host-owned local agent store.
///
/// Implementations return the **bare record**, never the wrapped input
/// envelope, and `Ok(None)` for a null result.
pub trait AgentStore: Send + Sync + 'static {
    /// Handle one store operation.
    fn call<'a>(&'a self, request: StoreRequest)
        -> BoxFuture<'a, HandlerResult<Option<JsonValue>>>;
}

/// The identifying key a record is stored under, per substore.
/// The in-memory state behind [`MemoryStore`].
#[derive(Default)]
struct State {
    /// `agentId` -> agent document.
    agents: BTreeMap<String, JsonValue>,
    /// `(agentId, runId)` -> run document. Runs are keyed by both, matching
    /// the bridge's own stores.
    runs: BTreeMap<(String, String), JsonValue>,
    /// `runId` -> the run's event log, in sequence order.
    run_events: BTreeMap<String, Vec<JsonValue>>,
    /// `(runId, idempotencyKey)` -> the event that key already produced.
    idempotency: BTreeMap<(String, String), JsonValue>,
    /// `(agentId, blobId)` -> base64 checkpoint bytes.
    checkpoints: BTreeMap<(String, String), String>,
}

/// An [`AgentStore`] that keeps everything in memory.
///
/// A faithful port of the bridge's own in-memory store, so it behaves the way
/// the built-in `SQLite` and JSONL stores do: `create` rejects duplicates,
/// `update` rejects unknown records and replaces the document wholesale,
/// `delete` takes a filter rather than a bare id, and every `list` returns
/// `{"items": [...]}` with a `nextCursor` (or `nextOffset` for run events) only
/// when another page exists.
///
/// Useful for tests and for short-lived programs that want no state on disk.
/// Everything is lost when the process exits.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<State>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("State")
            .field("agents", &self.agents.len())
            .field("runs", &self.runs.len())
            .field("run_events", &self.run_events.len())
            .field("checkpoints", &self.checkpoints.len())
            .finish()
    }
}

/// The default page size the bridge's stores use for records.
const DEFAULT_PAGE_LIMIT: usize = 50;
/// The default page size for run events.
const DEFAULT_EVENT_LIMIT: usize = 100;

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many agent documents are held.
    pub fn agent_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.agents.len())
            .unwrap_or(0)
    }

    /// How many run documents are held.
    pub fn run_count(&self) -> usize {
        self.state.lock().map(|state| state.runs.len()).unwrap_or(0)
    }

    /// How many events are recorded for a run.
    pub fn event_count(&self, run_id: &str) -> usize {
        self.state
            .lock()
            .map(|state| state.run_events.get(run_id).map_or(0, Vec::len))
            .unwrap_or(0)
    }

    /// Whether the store holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .map(|state| {
                state.agents.is_empty()
                    && state.runs.is_empty()
                    && state.run_events.is_empty()
                    && state.checkpoints.is_empty()
            })
            .unwrap_or(true)
    }

    fn handle(&self, request: StoreRequest) -> HandlerResult<Option<JsonValue>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "the store lock was poisoned")?;
        match (&request.substore, &request.method) {
            (Substore::Agents, method) => agents(&mut state, method, &request),
            (Substore::Runs, method) => runs(&mut state, method, &request),
            (Substore::RunEvents, method) => run_events(&mut state, method, &request),
            (Substore::Checkpoints, method) => checkpoints(&mut state, method, &request),
            (substore, method) => Err(format!(
                "MemoryStore does not implement {}.{}",
                substore.as_str(),
                method.as_str()
            )
            .into()),
        }
    }
}

impl AgentStore for MemoryStore {
    fn call<'a>(
        &'a self,
        request: StoreRequest,
    ) -> BoxFuture<'a, HandlerResult<Option<JsonValue>>> {
        let result = self.handle(request);
        Box::pin(async move { result })
    }
}

// ---- helpers ---------------------------------------------------------------

fn field<'a>(value: &'a JsonValue, name: &str) -> Option<&'a JsonValue> {
    value.get(name)
}

fn string_field(value: &JsonValue, name: &str) -> Option<String> {
    value.get(name)?.as_str().map(str::to_string)
}

fn require_string(value: &JsonValue, name: &str) -> HandlerResult<String> {
    string_field(value, name).ok_or_else(|| format!("the input is missing {name:?}").into())
}

/// The `filter` object a `list` or `delete` input carries, or an empty one.
fn filter(request: &StoreRequest) -> JsonValue {
    field(&request.input, "filter")
        .cloned()
        .unwrap_or(JsonValue::Object(Default::default()))
}

/// A `limit`, honoring the bridge's defaults.
fn limit_of(filter: &JsonValue, default: usize) -> usize {
    filter
        .get("limit")
        .and_then(JsonValue::as_u64)
        .map_or(default, |limit| limit as usize)
}

/// Whether a filter's string-array field either is absent or contains `value`.
fn id_matches(filter: &JsonValue, name: &str, value: &str) -> bool {
    match filter.get(name).and_then(JsonValue::as_array) {
        Some(ids) if !ids.is_empty() => ids.iter().any(|id| id.as_str() == Some(value)),
        _ => true,
    }
}

/// Page cursors are opaque to the bridge: it hands back exactly what we
/// returned. This matches the bridge's own encoding anyway — base64url JSON —
/// so a store swapped in mid-flight stays compatible.
fn encode_cursor(value: &JsonValue) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
}

fn decode_cursor(value: &str) -> HandlerResult<JsonValue> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|error| format!("invalid page cursor: {error}"))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Build the `{items, nextCursor?}` page every `list` returns.
fn page(
    items: Vec<JsonValue>,
    limit: usize,
    next_cursor: impl Fn(&JsonValue) -> JsonValue,
) -> JsonValue {
    let has_more = items.len() > limit;
    let page: Vec<JsonValue> = items.into_iter().take(limit).collect();
    let mut result = json!({ "items": page.clone() });
    // A cursor is only emitted when there is genuinely another page.
    if has_more {
        if let Some(last) = page.last() {
            result["nextCursor"] = json!(encode_cursor(&next_cursor(last)));
        }
    }
    result
}

fn number(value: &JsonValue, name: &str) -> i64 {
    value.get(name).and_then(JsonValue::as_i64).unwrap_or(0)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

// ---- agents ----------------------------------------------------------------

fn agents(
    state: &mut State,
    method: &StoreMethod,
    request: &StoreRequest,
) -> HandlerResult<Option<JsonValue>> {
    match method {
        StoreMethod::Get => {
            let agent_id = require_string(&request.input, "agentId")?;
            Ok(state.agents.get(&agent_id).cloned())
        }
        StoreMethod::Create => {
            let agent = request.record().clone();
            let agent_id = require_string(&agent, "agentId")?;
            if state.agents.contains_key(&agent_id) {
                return Err(format!("Agent {agent_id} already exists").into());
            }
            state.agents.insert(agent_id.clone(), agent);
            // The bare record, never the wrapped input envelope.
            Ok(state.agents.get(&agent_id).cloned())
        }
        StoreMethod::Update => {
            let agent = request.record().clone();
            let agent_id = require_string(&agent, "agentId")?;
            if !state.agents.contains_key(&agent_id) {
                return Err(format!("Agent {agent_id} not found").into());
            }
            // The bridge's stores replace the document wholesale rather than
            // merging changed fields into it.
            state.agents.insert(agent_id.clone(), agent);
            Ok(state.agents.get(&agent_id).cloned())
        }
        StoreMethod::Delete => {
            let filter = filter(request);
            let cwd = filter.get("cwd");
            let doomed: Vec<String> = state
                .agents
                .iter()
                .filter(|(agent_id, agent)| {
                    id_matches(&filter, "agentIds", agent_id)
                        && cwd.is_none_or(|cwd| agent.get("cwd") == Some(cwd))
                })
                .map(|(agent_id, _)| agent_id.clone())
                .collect();
            if doomed.is_empty() {
                return Err("No agents matched delete filter".into());
            }
            for agent_id in doomed {
                state.agents.remove(&agent_id);
            }
            Ok(None)
        }
        StoreMethod::List => {
            let filter = filter(request);
            let cwd = filter.get("cwd");
            let mut items: Vec<JsonValue> = state
                .agents
                .values()
                .filter(|agent| cwd.is_none_or(|cwd| agent.get("cwd") == Some(cwd)))
                .cloned()
                .collect();
            // Newest first: updatedAt descending, then agentId descending.
            items.sort_by(|a, b| {
                number(b, "updatedAt")
                    .cmp(&number(a, "updatedAt"))
                    .then_with(|| string_field(b, "agentId").cmp(&string_field(a, "agentId")))
            });
            if let Some(cursor) = filter.get("cursor").and_then(JsonValue::as_str) {
                let cursor = decode_cursor(cursor)?;
                let (updated_at, agent_id) = (
                    number(&cursor, "updatedAt"),
                    string_field(&cursor, "agentId"),
                );
                items.retain(|agent| {
                    number(agent, "updatedAt") < updated_at
                        || (number(agent, "updatedAt") == updated_at
                            && string_field(agent, "agentId") < agent_id)
                });
            }
            Ok(Some(page(
                items,
                limit_of(&filter, DEFAULT_PAGE_LIMIT),
                |last| json!({"updatedAt": number(last, "updatedAt"), "agentId": string_field(last, "agentId")}),
            )))
        }
        other => Err(format!("MemoryStore does not implement agents.{}", other.as_str()).into()),
    }
}

// ---- runs ------------------------------------------------------------------

fn run_key(value: &JsonValue) -> HandlerResult<(String, String)> {
    Ok((
        require_string(value, "agentId")?,
        require_string(value, "runId")?,
    ))
}

fn runs(
    state: &mut State,
    method: &StoreMethod,
    request: &StoreRequest,
) -> HandlerResult<Option<JsonValue>> {
    match method {
        StoreMethod::Get => {
            let key = run_key(&request.input)?;
            Ok(state.runs.get(&key).cloned())
        }
        StoreMethod::Create => {
            let run = request.record().clone();
            let key = run_key(&run)?;
            if state.runs.contains_key(&key) {
                return Err(format!("Run {} already exists for agent {}", key.1, key.0).into());
            }
            state.runs.insert(key.clone(), run);
            Ok(state.runs.get(&key).cloned())
        }
        StoreMethod::Update => {
            let run = request.record().clone();
            let key = run_key(&run)?;
            if !state.runs.contains_key(&key) {
                return Err(format!("Run {} not found for agent {}", key.1, key.0).into());
            }
            state.runs.insert(key.clone(), run);
            Ok(state.runs.get(&key).cloned())
        }
        StoreMethod::Delete => {
            let filter = filter(request);
            let doomed: Vec<(String, String)> = state
                .runs
                .keys()
                .filter(|(agent_id, run_id)| {
                    id_matches(&filter, "agentIds", agent_id)
                        && id_matches(&filter, "runIds", run_id)
                })
                .cloned()
                .collect();
            for key in doomed {
                state.runs.remove(&key);
            }
            Ok(None)
        }
        StoreMethod::List => {
            let filter = filter(request);
            let mut items: Vec<JsonValue> = state
                .runs
                .iter()
                .filter(|((agent_id, run_id), _)| {
                    id_matches(&filter, "agentIds", agent_id)
                        && id_matches(&filter, "runIds", run_id)
                })
                .map(|(_, run)| run.clone())
                .collect();
            // Oldest first: turnNumber ascending, then runId ascending.
            items.sort_by(|a, b| {
                number(a, "turnNumber")
                    .cmp(&number(b, "turnNumber"))
                    .then_with(|| string_field(a, "runId").cmp(&string_field(b, "runId")))
            });
            if let Some(cursor) = filter.get("cursor").and_then(JsonValue::as_str) {
                let cursor = decode_cursor(cursor)?;
                let (turn, run_id) = (
                    number(&cursor, "turnNumber"),
                    string_field(&cursor, "runId"),
                );
                items.retain(|run| {
                    number(run, "turnNumber") > turn
                        || (number(run, "turnNumber") == turn
                            && string_field(run, "runId") > run_id)
                });
            }
            Ok(Some(page(
                items,
                limit_of(&filter, DEFAULT_PAGE_LIMIT),
                |last| json!({"turnNumber": number(last, "turnNumber"), "runId": string_field(last, "runId")}),
            )))
        }
        other => Err(format!("MemoryStore does not implement runs.{}", other.as_str()).into()),
    }
}

// ---- run events ------------------------------------------------------------

/// Run event offsets are the sequence number as a decimal string.
fn parse_offset(value: Option<&str>) -> HandlerResult<i64> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(0);
    };
    value
        .parse::<i64>()
        .ok()
        .filter(|parsed| *parsed >= 0)
        .ok_or_else(|| format!("Invalid run event offset {value}").into())
}

fn run_events(
    state: &mut State,
    method: &StoreMethod,
    request: &StoreRequest,
) -> HandlerResult<Option<JsonValue>> {
    match method {
        StoreMethod::Append => {
            let run_id = require_string(&request.input, "runId")?;
            let idempotency_key = string_field(&request.input, "idempotencyKey");

            // A repeated append under the same key returns the original event
            // rather than recording a second one.
            if let Some(key) = &idempotency_key {
                if let Some(existing) = state.idempotency.get(&(run_id.clone(), key.clone())) {
                    return Ok(Some(existing.clone()));
                }
            }

            let events = state.run_events.entry(run_id.clone()).or_default();
            let seq = events.last().map_or(0, |event| number(event, "seq")) + 1;
            let event = json!({
                "runId": run_id,
                "seq": seq,
                "offset": seq.to_string(),
                "eventType": field(&request.input, "eventType").cloned().unwrap_or(JsonValue::Null),
                "payload": field(&request.input, "payload").cloned().unwrap_or(JsonValue::Null),
                "payloadRef": field(&request.input, "payloadRef").cloned().unwrap_or(JsonValue::Null),
                "idempotencyKey": idempotency_key.clone().map_or(JsonValue::Null, JsonValue::String),
                "createdAt": now_millis(),
            });
            events.push(event.clone());
            if let Some(key) = idempotency_key {
                state.idempotency.insert((run_id, key), event.clone());
            }
            Ok(Some(event))
        }
        StoreMethod::List => {
            let run_id = require_string(&request.input, "runId")?;
            let after = parse_offset(request.input.get("afterOffset").and_then(JsonValue::as_str))?;
            let limit = limit_of(&request.input, DEFAULT_EVENT_LIMIT);

            let matching: Vec<JsonValue> = state
                .run_events
                .get(&run_id)
                .map(|events| {
                    events
                        .iter()
                        .filter(|event| number(event, "seq") > after)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();

            let items: Vec<JsonValue> = matching.iter().take(limit).cloned().collect();
            let mut result = json!({"items": items.clone()});
            // Run events resume by offset, not by an opaque cursor.
            if matching.len() > items.len() {
                if let Some(last) = items.last() {
                    result["nextOffset"] =
                        field(last, "offset").cloned().unwrap_or(JsonValue::Null);
                }
            }
            Ok(Some(result))
        }
        StoreMethod::Delete => {
            let filter = filter(request);
            let doomed: Vec<String> = state
                .run_events
                .keys()
                .filter(|run_id| id_matches(&filter, "runIds", run_id))
                .cloned()
                .collect();
            for run_id in doomed {
                state.run_events.remove(&run_id);
                state
                    .idempotency
                    .retain(|(recorded, _), _| recorded != &run_id);
            }
            Ok(None)
        }
        other => Err(format!(
            "MemoryStore does not implement runEvents.{}",
            other.as_str()
        )
        .into()),
    }
}

// ---- checkpoints -----------------------------------------------------------

fn checkpoint_key(value: &JsonValue) -> HandlerResult<(String, String)> {
    Ok((
        require_string(value, "agentId")?,
        require_string(value, "blobId")?,
    ))
}

fn checkpoints(
    state: &mut State,
    method: &StoreMethod,
    request: &StoreRequest,
) -> HandlerResult<Option<JsonValue>> {
    match method {
        StoreMethod::Get => {
            let key = checkpoint_key(&request.input)?;
            let found = state.checkpoints.get(&key);
            // Checkpoint reads are explicitly shaped {found, data}, with the
            // blob bytes base64-encoded.
            Ok(Some(json!({
                "found": found.is_some(),
                "data": found.cloned().map_or(JsonValue::Null, JsonValue::String),
            })))
        }
        StoreMethod::Create => {
            let key = checkpoint_key(&request.input)?;
            if state.checkpoints.contains_key(&key) {
                return Err(format!(
                    "Checkpoint blob {} already exists for agent {}",
                    key.1, key.0
                )
                .into());
            }
            let data = require_string(&request.input, "data")?;
            state.checkpoints.insert(key, data);
            Ok(None)
        }
        StoreMethod::Update => {
            let key = checkpoint_key(&request.input)?;
            if !state.checkpoints.contains_key(&key) {
                return Err(
                    format!("Checkpoint blob {} not found for agent {}", key.1, key.0).into(),
                );
            }
            let data = require_string(&request.input, "data")?;
            state.checkpoints.insert(key, data);
            Ok(None)
        }
        StoreMethod::Delete => {
            let filter = filter(request);
            let doomed: Vec<(String, String)> = state
                .checkpoints
                .keys()
                .filter(|(agent_id, blob_id)| {
                    id_matches(&filter, "agentIds", agent_id)
                        && id_matches(&filter, "blobIds", blob_id)
                })
                .cloned()
                .collect();
            for key in doomed {
                state.checkpoints.remove(&key);
            }
            Ok(None)
        }
        StoreMethod::List => {
            let filter = filter(request);
            // Checkpoints list blob ids, not records.
            let mut blob_ids: Vec<JsonValue> = state
                .checkpoints
                .keys()
                .filter(|(agent_id, _)| id_matches(&filter, "agentIds", agent_id))
                .map(|(_, blob_id)| json!(blob_id))
                .collect();
            blob_ids.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            blob_ids.dedup();
            if let Some(cursor) = filter.get("cursor").and_then(JsonValue::as_str) {
                blob_ids.retain(|blob_id| blob_id.as_str().unwrap_or_default() > cursor);
            }

            let limit = limit_of(&filter, DEFAULT_PAGE_LIMIT);
            let has_more = blob_ids.len() > limit;
            let items: Vec<JsonValue> = blob_ids.into_iter().take(limit).collect();
            let mut result = json!({"items": items.clone()});
            if has_more {
                if let Some(last) = items.last() {
                    // A checkpoint cursor is the blob id itself, not encoded.
                    result["nextCursor"] = last.clone();
                }
            }
            Ok(Some(result))
        }
        other => Err(format!(
            "MemoryStore does not implement checkpoints.{}",
            other.as_str()
        )
        .into()),
    }
}

/// Wraps another store and traces every operation.
///
/// `docs/services.md` recommends logging live traffic from one real agent turn
/// before writing a store: a single `CreateAgent` + `Send` exercises most
/// substores and methods. Wrap [`MemoryStore`] in this, run a turn, and read
/// the shapes off the `cursor_sdk::store` target at `debug` level.
pub struct LoggingStore<S> {
    inner: S,
}

impl<S: AgentStore> LoggingStore<S> {
    /// Wrap a store.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: AgentStore> AgentStore for LoggingStore<S> {
    fn call<'a>(
        &'a self,
        request: StoreRequest,
    ) -> BoxFuture<'a, HandlerResult<Option<JsonValue>>> {
        Box::pin(async move {
            let label = format!("{}.{}", request.substore.as_str(), request.method.as_str());
            tracing::debug!(
                target: "cursor_sdk::store",
                "-> {label} {}", request.input
            );
            let result = self.inner.call(request).await;
            match &result {
                Ok(Some(output)) => {
                    tracing::debug!(target: "cursor_sdk::store", "<- {label} {output}")
                }
                Ok(None) => tracing::debug!(target: "cursor_sdk::store", "<- {label} null"),
                Err(error) => {
                    tracing::debug!(target: "cursor_sdk::store", "<- {label} error: {error}")
                }
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, cwd: &str, updated_at: i64) -> JsonValue {
        json!({"agentId": id, "cwd": cwd, "updatedAt": updated_at, "name": "demo"})
    }

    async fn call(
        store: &MemoryStore,
        substore: &str,
        method: &str,
        input: JsonValue,
    ) -> Option<JsonValue> {
        store
            .call(StoreRequest::new(substore, method, input))
            .await
            .expect("the operation succeeds")
    }

    async fn fail(store: &MemoryStore, substore: &str, method: &str, input: JsonValue) -> String {
        store
            .call(StoreRequest::new(substore, method, input))
            .await
            .expect_err("the operation fails")
            .to_string()
    }

    // ---- agents -----------------------------------------------------------

    #[tokio::test]
    async fn create_returns_the_bare_record_not_the_wrapper() {
        let store = MemoryStore::new();
        let output = call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;
        // Echoing {"agent": …} back is what makes the bridge fail opaquely.
        assert_eq!(output, Some(agent("a1", "/repo", 1)));
    }

    #[tokio::test]
    async fn get_round_trips_and_a_miss_is_none() {
        let store = MemoryStore::new();
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;

        assert_eq!(
            call(&store, "agents", "get", json!({"agentId": "a1"})).await,
            Some(agent("a1", "/repo", 1))
        );
        assert_eq!(
            call(&store, "agents", "get", json!({"agentId": "absent"})).await,
            None,
            "a get miss is a null result"
        );
    }

    #[tokio::test]
    async fn create_rejects_a_duplicate_and_update_rejects_a_stranger() {
        let store = MemoryStore::new();
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;

        let error = fail(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 2)}),
        )
        .await;
        assert!(error.contains("already exists"), "{error}");

        let error = fail(
            &store,
            "agents",
            "update",
            json!({"agent": agent("ghost", "/repo", 1)}),
        )
        .await;
        assert!(error.contains("not found"), "{error}");
    }

    #[tokio::test]
    async fn update_replaces_the_document_wholesale() {
        let store = MemoryStore::new();
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;

        // The bridge's own stores replace rather than merge, so a field the
        // update omits is gone.
        let updated = call(
            &store,
            "agents",
            "update",
            json!({"agent": {"agentId": "a1", "cwd": "/repo", "updatedAt": 2}}),
        )
        .await;
        assert_eq!(
            updated,
            Some(json!({"agentId": "a1", "cwd": "/repo", "updatedAt": 2}))
        );
        assert_eq!(updated.unwrap().get("name"), None);
    }

    #[tokio::test]
    async fn delete_takes_a_filter_and_returns_no_output() {
        let store = MemoryStore::new();
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a2", "/other", 2)}),
        )
        .await;

        // Not a bare {"agentId": …}: deletes are filtered.
        let output = call(
            &store,
            "agents",
            "delete",
            json!({"filter": {"agentIds": ["a1"]}}),
        )
        .await;
        assert_eq!(output, None);
        assert_eq!(store.agent_count(), 1);

        let error = fail(
            &store,
            "agents",
            "delete",
            json!({"filter": {"agentIds": ["gone"]}}),
        )
        .await;
        assert!(error.contains("No agents matched"), "{error}");
    }

    #[tokio::test]
    async fn agents_list_returns_items_newest_first() {
        let store = MemoryStore::new();
        for (id, updated) in [("a1", 10), ("a2", 30), ("a3", 20)] {
            call(
                &store,
                "agents",
                "create",
                json!({"agent": agent(id, "/repo", updated)}),
            )
            .await;
        }

        let page = call(&store, "agents", "list", json!({})).await.unwrap();
        let ids: Vec<&str> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|agent| agent["agentId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a2", "a3", "a1"], "updatedAt descending");
        assert_eq!(page.get("nextCursor"), None, "one page, so no cursor");
    }

    #[tokio::test]
    async fn agents_list_filters_by_cwd() {
        let store = MemoryStore::new();
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a2", "/elsewhere", 2)}),
        )
        .await;

        let page = call(
            &store,
            "agents",
            "list",
            json!({"filter": {"cwd": "/repo"}}),
        )
        .await
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["agentId"], json!("a1"));
    }

    #[tokio::test]
    async fn agents_list_paginates_through_its_own_cursor() {
        let store = MemoryStore::new();
        for (id, updated) in [("a1", 10), ("a2", 30), ("a3", 20)] {
            call(
                &store,
                "agents",
                "create",
                json!({"agent": agent(id, "/repo", updated)}),
            )
            .await;
        }

        let first = call(&store, "agents", "list", json!({"filter": {"limit": 2}}))
            .await
            .unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        let cursor = first["nextCursor"].as_str().expect("another page exists");

        let second = call(
            &store,
            "agents",
            "list",
            json!({"filter": {"limit": 2, "cursor": cursor}}),
        )
        .await
        .unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["items"][0]["agentId"], json!("a1"));
        assert_eq!(
            second.get("nextCursor"),
            None,
            "the last page has no cursor"
        );
    }

    // ---- runs -------------------------------------------------------------

    #[tokio::test]
    async fn runs_are_keyed_by_agent_and_run() {
        let store = MemoryStore::new();
        call(
            &store,
            "runs",
            "create",
            json!({"run": {"agentId": "a1", "runId": "r1", "turnNumber": 1}}),
        )
        .await;
        // The same run id under a different agent is a different record.
        call(
            &store,
            "runs",
            "create",
            json!({"run": {"agentId": "a2", "runId": "r1", "turnNumber": 1}}),
        )
        .await;
        assert_eq!(store.run_count(), 2);

        assert_eq!(
            call(
                &store,
                "runs",
                "get",
                json!({"agentId": "a1", "runId": "r1"})
            )
            .await,
            Some(json!({"agentId": "a1", "runId": "r1", "turnNumber": 1}))
        );
    }

    #[tokio::test]
    async fn runs_list_is_oldest_first_and_filters_by_agent() {
        let store = MemoryStore::new();
        for (agent_id, run_id, turn) in [("a1", "r2", 2), ("a1", "r1", 1), ("a2", "r9", 1)] {
            call(
                &store,
                "runs",
                "create",
                json!({"run": {"agentId": agent_id, "runId": run_id, "turnNumber": turn}}),
            )
            .await;
        }

        let page = call(
            &store,
            "runs",
            "list",
            json!({"filter": {"agentIds": ["a1"]}}),
        )
        .await
        .unwrap();
        let ids: Vec<&str> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["runId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["r1", "r2"], "turnNumber ascending");
    }

    // ---- run events -------------------------------------------------------

    #[tokio::test]
    async fn append_assigns_a_sequence_and_a_string_offset() {
        let store = MemoryStore::new();
        let first = call(
            &store,
            "runEvents",
            "append",
            json!({"runId": "r1", "eventType": "assistant", "payload": {"text": "hi"}}),
        )
        .await
        .unwrap();

        assert_eq!(first["runId"], json!("r1"));
        assert_eq!(first["seq"], json!(1));
        // Offsets are the sequence number as a decimal string.
        assert_eq!(first["offset"], json!("1"));
        assert_eq!(first["eventType"], json!("assistant"));
        assert!(first["createdAt"].is_i64());

        let second = call(
            &store,
            "runEvents",
            "append",
            json!({"runId": "r1", "eventType": "status", "payload": {}}),
        )
        .await
        .unwrap();
        assert_eq!(second["offset"], json!("2"));
        assert_eq!(store.event_count("r1"), 2);
    }

    #[tokio::test]
    async fn append_is_idempotent_under_a_key() {
        let store = MemoryStore::new();
        let input = json!({"runId": "r1", "eventType": "assistant", "idempotencyKey": "k1"});
        let first = call(&store, "runEvents", "append", input.clone())
            .await
            .unwrap();
        let repeat = call(&store, "runEvents", "append", input).await.unwrap();

        assert_eq!(first, repeat, "a retry returns the original event");
        assert_eq!(store.event_count("r1"), 1, "and records nothing new");
    }

    #[tokio::test]
    async fn run_events_list_resumes_after_an_offset() {
        let store = MemoryStore::new();
        for index in 0..5 {
            call(
                &store,
                "runEvents",
                "append",
                json!({"runId": "r1", "eventType": "assistant", "payload": {"n": index}}),
            )
            .await;
        }

        let all = call(&store, "runEvents", "list", json!({"runId": "r1"}))
            .await
            .unwrap();
        assert_eq!(all["items"].as_array().unwrap().len(), 5);
        assert_eq!(all.get("nextOffset"), None);

        // afterOffset is exclusive.
        let resumed = call(
            &store,
            "runEvents",
            "list",
            json!({"runId": "r1", "afterOffset": "3"}),
        )
        .await
        .unwrap();
        assert_eq!(resumed["items"].as_array().unwrap().len(), 2);
        assert_eq!(resumed["items"][0]["offset"], json!("4"));
    }

    #[tokio::test]
    async fn run_events_list_reports_the_next_offset_when_truncated() {
        let store = MemoryStore::new();
        for _ in 0..5 {
            call(
                &store,
                "runEvents",
                "append",
                json!({"runId": "r1", "eventType": "e"}),
            )
            .await;
        }

        let page = call(
            &store,
            "runEvents",
            "list",
            json!({"runId": "r1", "limit": 2}),
        )
        .await
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        // Run events resume by offset, not by an opaque cursor.
        assert_eq!(page["nextOffset"], json!("2"));
    }

    #[tokio::test]
    async fn run_events_delete_takes_a_run_id_filter() {
        let store = MemoryStore::new();
        call(
            &store,
            "runEvents",
            "append",
            json!({"runId": "r1", "eventType": "e"}),
        )
        .await;
        call(
            &store,
            "runEvents",
            "append",
            json!({"runId": "r2", "eventType": "e"}),
        )
        .await;

        let output = call(
            &store,
            "runEvents",
            "delete",
            json!({"filter": {"runIds": ["r1"]}}),
        )
        .await;
        assert_eq!(output, None);
        assert_eq!(store.event_count("r1"), 0);
        assert_eq!(store.event_count("r2"), 1);
    }

    #[tokio::test]
    async fn a_bad_offset_is_rejected() {
        let store = MemoryStore::new();
        let error = fail(
            &store,
            "runEvents",
            "list",
            json!({"runId": "r1", "afterOffset": "abc"}),
        )
        .await;
        assert!(error.contains("Invalid run event offset"), "{error}");
    }

    // ---- checkpoints ------------------------------------------------------

    #[tokio::test]
    async fn checkpoint_get_reports_found_and_base64_data() {
        let store = MemoryStore::new();
        call(
            &store,
            "checkpoints",
            "create",
            json!({"agentId": "a1", "blobId": "b1", "data": "aGVsbG8="}),
        )
        .await;

        assert_eq!(
            call(
                &store,
                "checkpoints",
                "get",
                json!({"agentId": "a1", "blobId": "b1"})
            )
            .await,
            Some(json!({"found": true, "data": "aGVsbG8="}))
        );
        assert_eq!(
            call(
                &store,
                "checkpoints",
                "get",
                json!({"agentId": "a1", "blobId": "absent"})
            )
            .await,
            Some(json!({"found": false, "data": null}))
        );
    }

    #[tokio::test]
    async fn checkpoints_list_returns_blob_ids() {
        let store = MemoryStore::new();
        for blob_id in ["b2", "b1"] {
            call(
                &store,
                "checkpoints",
                "create",
                json!({"agentId": "a1", "blobId": blob_id, "data": "eA=="}),
            )
            .await;
        }

        // Checkpoints list ids, not records.
        let page = call(&store, "checkpoints", "list", json!({}))
            .await
            .unwrap();
        assert_eq!(page, json!({"items": ["b1", "b2"]}));
    }

    #[tokio::test]
    async fn checkpoint_writes_follow_create_then_update() {
        let store = MemoryStore::new();
        let error = fail(
            &store,
            "checkpoints",
            "update",
            json!({"agentId": "a1", "blobId": "b1", "data": "eA=="}),
        )
        .await;
        assert!(error.contains("not found"), "{error}");

        call(
            &store,
            "checkpoints",
            "create",
            json!({"agentId": "a1", "blobId": "b1", "data": "eA=="}),
        )
        .await;
        let error = fail(
            &store,
            "checkpoints",
            "create",
            json!({"agentId": "a1", "blobId": "b1", "data": "eQ=="}),
        )
        .await;
        assert!(error.contains("already exists"), "{error}");
    }

    // ---- shape ------------------------------------------------------------

    #[tokio::test]
    async fn every_list_wraps_its_array_in_an_items_object() {
        // A CallStore output is a protobuf Struct, which can only encode an
        // object — so no list can return a bare array.
        let store = MemoryStore::new();
        for substore in ["agents", "runs", "checkpoints"] {
            let page = call(&store, substore, "list", json!({})).await.unwrap();
            assert!(page.is_object(), "{substore}.list must be an object");
            assert!(page["items"].is_array(), "{substore}.list needs items");
        }
        let events = call(&store, "runEvents", "list", json!({"runId": "r1"}))
            .await
            .unwrap();
        assert!(events["items"].is_array());
    }

    #[test]
    fn unknown_substores_and_methods_stay_readable() {
        let request = StoreRequest::new("futureThing", "upsert", json!({}));
        assert_eq!(request.substore.as_str(), "futureThing");
        assert_eq!(request.method.as_str(), "upsert");
    }

    #[tokio::test]
    async fn an_unimplemented_operation_says_so() {
        let store = MemoryStore::new();
        let error = fail(&store, "futureThing", "upsert", json!({})).await;
        assert!(
            error.contains("does not implement futureThing.upsert"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_empty_store_reports_itself_empty() {
        let store = MemoryStore::new();
        assert!(store.is_empty());
        call(
            &store,
            "agents",
            "create",
            json!({"agent": agent("a1", "/repo", 1)}),
        )
        .await;
        assert!(!store.is_empty());
    }
}
