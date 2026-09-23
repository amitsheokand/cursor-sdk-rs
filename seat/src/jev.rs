//! Native Jev System One client and the seat's custom tools (P6).
//!
//! Mirrors `gates/client.py` (endpoint, pinned model, fail-closed
//! `evaluate`) and `runner/keys.py` (env-first key, 0600 env file). The
//! key lives behind a redacting newtype and is never logged.
//!
//! Question shapes live in TOML config under `questions_dir`, never in
//! code. Supported file shape mirrors `gates/questions/*.toml`:
//! `[question.<id>]` tables with `type`, `instructions`, and — per type —
//! `options` (choice label list, `key: description` like triage.toml) or
//! `criteria_<opt>` keys, `criteria_true`/`criteria_false` (noul), or
//! `levels` (score); plus an optional `[thresholds]` table. Files are
//! merged; duplicate ids are an error.
//!
//! Tools: `skill_use` (deferred `SKILL.md`), `jev_verify`
//! (citation-check: string-match `fabricated` path plus a Choice), and
//! `jev_screen` (guardrails: Noul battery + severity Score with uniform
//! thresholds, since hazards are config-defined). Anything unavailable
//! stays declared and answers `not configured`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Pinned model, mirroring `gates/client.py::JEV_MODEL`.
pub const JEV_MODEL: &str = "jev-1.13.0";
/// Endpoint, mirroring `SYSTEM_ONE_URL` (`TYPESAFE_ENDPOINT` overrides).
pub const SYSTEM_ONE_URL: &str = "https://api.typesafe.ai/v1/systemone";
/// Request timeout, mirroring `JEV_FETCH_TIMEOUT_S`.
pub const JEV_TIMEOUT: Duration = Duration::from_secs(4);
/// Key file, mirroring `runner/keys.py::TYPESAFE_ENV`.
pub const TYPESAFE_ENV_REL: &str = ".config/typesafe.env";

/// Seat tool names (always registered; see [`SeatTools`]).
pub const TOOL_SKILL_USE: &str = "skill_use";
pub const TOOL_JEV_VERIFY: &str = "jev_verify";
pub const TOOL_JEV_SCREEN: &str = "jev_screen";

/// Shared descriptions (single source for schemas and prune inventory).
pub const TOOL_SKILL_USE_DESC: &str =
    "Load a skill's SKILL.md file when the task matches its description";
pub const TOOL_JEV_VERIFY_DESC: &str =
    "Check whether a source section supports a claim (verified|contradicted|unsupported|fabricated)";
pub const TOOL_JEV_SCREEN_DESC: &str =
    "Screen a message with the guardrail battery (pass|review|block)";

/// All seat-owned custom tools in one place.
pub const SEAT_TOOLS: [(&str, &str); 3] = [
    (TOOL_SKILL_USE, TOOL_SKILL_USE_DESC),
    (TOOL_JEV_VERIFY, TOOL_JEV_VERIFY_DESC),
    (TOOL_JEV_SCREEN, TOOL_JEV_SCREEN_DESC),
];

/// Default question ids (config keys, not shapes).
pub const CHECK_VERIFY: &str = "relation";
pub const CHECK_TRIAGE: &str = "triage";
pub const CHECK_SELF: &str = "receipt_supported";

/// Self-check pass boundary when no `receipt_supported_at` threshold is
/// configured (mirrors Python post-gate convention `<id>_at`).
pub const SELF_CHECK_PASS_AT: f64 = 0.5;

/// Pass boundary for a question id: `<id>_at` from `[thresholds]`,
/// else `fallback`. Thresholds are config, not code.
pub fn threshold_for(set: &QuestionSet, id: &str, fallback: f64) -> f64 {
    set.thresholds
        .get(&format!("{id}_at"))
        .copied()
        .unwrap_or(fallback)
}
/// Screen routing thresholds (strict policy from the guardrails cookbook).
pub const SCREEN_REVIEW_AT: f64 = 0.35;
pub const SCREEN_ACTION_AT: f64 = 0.70;
pub const SCREEN_SEVERITY_BLOCK: f64 = 2.0;
/// Skill bodies are capped; the rest is truncated with a note.
pub const MAX_SKILL_BYTES: u64 = 65536;
/// Self-check diff input is capped.
pub const MAX_DIFF_CHARS: usize = 8000;

/// Jev failures. Key material never appears in these messages.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JevError {
    #[error("TYPESAFE_API_KEY is not set and no key file was found")]
    NoKey,
    #[error("key file {0} must be mode 0600")]
    BadKeyMode(String),
    #[error("key file problem: {0}")]
    KeyFile(String),
    #[error("jev HTTP transport failed: {0}")]
    Transport(String),
    #[error("jev HTTP {0}")]
    Status(u16),
    #[error("jev protocol error: {0}")]
    Protocol(String),
    #[error("jev config error: {0}")]
    Config(String),
}

/// API key with redacted `Debug` (per `obs-no-sensitive-data`).
#[derive(Clone)]
pub struct ApiKey(String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey([redacted])")
    }
}

impl ApiKey {
    /// `TYPESAFE_API_KEY` wins; else `~/.config/typesafe.env`
    /// (`KEY=VALUE`, comments/`export` tolerated like keys.py), which
    /// must be mode 0600.
    pub fn load() -> Result<Self, JevError> {
        if let Ok(key) = std::env::var("TYPESAFE_API_KEY") {
            if !key.trim().is_empty() {
                return Ok(ApiKey(key));
            }
        }
        let home = std::env::var("HOME").map_err(|_| JevError::NoKey)?;
        let path = Path::new(&home).join(TYPESAFE_ENV_REL);
        Ok(ApiKey(parse_key_file(&path, "TYPESAFE_API_KEY")?))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

fn parse_key_file(path: &Path, var: &str) -> Result<String, JevError> {
    let meta = std::fs::metadata(path).map_err(|_| JevError::NoKey)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o777 != 0o600 {
            return Err(JevError::BadKeyMode(path.display().to_string()));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| JevError::KeyFile(e.to_string()))?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().strip_prefix("export ").unwrap_or(name.trim()).trim();
        if name == var {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if value.is_empty() {
                break;
            }
            return Ok(value.to_string());
        }
    }
    Err(JevError::KeyFile(format!(
        "{var} not set in {}",
        path.display()
    )))
}

// ---- questions ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionType {
    Choice,
    Noul,
    Score,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Criteria {
    Map(BTreeMap<String, Option<String>>),
    List(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub qtype: QuestionType,
    pub instructions: String,
    pub criteria: Criteria,
}

impl Question {
    /// Render to the System One API shape.
    pub fn to_api_json(&self) -> serde_json::Value {
        let qtype = match self.qtype {
            QuestionType::Choice => "choice",
            QuestionType::Noul => "noul",
            QuestionType::Score => "score",
        };
        let criteria = match &self.criteria {
            Criteria::Map(map) => serde_json::to_value(map).unwrap_or_default(),
            Criteria::List(list) => serde_json::to_value(list).unwrap_or_default(),
        };
        serde_json::json!({
            "type": qtype,
            "instructions": self.instructions,
            "criteria": criteria,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct QuestionSet {
    pub questions: BTreeMap<String, Question>,
    pub thresholds: BTreeMap<String, f64>,
}

/// Load and validate every `*.toml` file in `dir`, merged by question id.
pub fn load_questions(dir: &Path) -> Result<QuestionSet, JevError> {
    let mut set = QuestionSet::default();
    let mut files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| JevError::Config(format!("cannot list {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| JevError::Config(format!("cannot list {}: {e}", dir.display())))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(JevError::Config(format!(
            "no .toml questions in {}",
            dir.display()
        )));
    }
    for path in &files {
        merge_toml_file(&mut set, path)?;
    }
    if set.questions.is_empty() {
        return Err(JevError::Config(format!(
            "no [question.*] tables in {}",
            dir.display()
        )));
    }
    Ok(set)
}

fn merge_toml_file(set: &mut QuestionSet, path: &Path) -> Result<(), JevError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| JevError::Config(format!("cannot read {}: {e}", path.display())))?;
    let value: toml::Value = text
        .parse()
        .map_err(|e| JevError::Config(format!("invalid TOML {}: {e}", path.display())))?;
    let table = value
        .as_table()
        .ok_or_else(|| JevError::Config(format!("{} is not a TOML table", path.display())))?;
    for (key, section) in table {
        match key.as_str() {
            "question" => {
                let questions = section.as_table().ok_or_else(|| {
                    JevError::Config(format!("[question] is not a table in {}", path.display()))
                })?;
                for (id, raw) in questions {
                    if set.questions.contains_key(id) {
                        return Err(JevError::Config(format!(
                            "duplicate question `{id}` in {}",
                            path.display()
                        )));
                    }
                    set.questions.insert(id.clone(), parse_question(id, raw, path)?);
                }
            }
            "thresholds" => {
                let thresholds = section.as_table().ok_or_else(|| {
                    JevError::Config(format!("[thresholds] is not a table in {}", path.display()))
                })?;
                for (key, value) in thresholds {
                    if set.thresholds.contains_key(key) {
                        return Err(JevError::Config(format!(
                            "duplicate threshold `{key}` in {}",
                            path.display()
                        )));
                    }
                    let number = value.as_float().or_else(|| value.as_integer().map(|v| v as f64)).ok_or_else(|| {
                        JevError::Config(format!(
                            "threshold `{key}` is not a number in {}",
                            path.display()
                        ))
                    })?;
                    set.thresholds.insert(key.clone(), number);
                }
            }
            other => {
                return Err(JevError::Config(format!(
                    "unknown table `{other}` in {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn parse_question(id: &str, raw: &toml::Value, path: &Path) -> Result<Question, JevError> {
    let fail = |reason: &str| JevError::Config(format!("question `{id}` in {}: {reason}", path.display()));
    let table = raw.as_table().ok_or_else(|| fail("not a table"))?;
    let qtype = match table.get("type").and_then(|v| v.as_str()) {
        Some("choice") => QuestionType::Choice,
        Some("noul") => QuestionType::Noul,
        Some("score") => QuestionType::Score,
        _ => return Err(fail("type must be choice|noul|score")),
    };
    let instructions = table
        .get("instructions")
        .and_then(|v| v.as_str())
        .ok_or_else(|| fail("missing instructions"))?
        .to_string();
    let criteria = match qtype {
        QuestionType::Choice => {
            let mut map: BTreeMap<String, Option<String>> = BTreeMap::new();
            if let Some(options) = table.get("options") {
                let options = options.as_array().ok_or_else(|| fail("options must be an array"))?;
                for option in options {
                    let text = option.as_str().ok_or_else(|| fail("option must be a string"))?;
                    map.insert(option_key(text), Some(text.to_string()));
                }
            }
            for (key, value) in table {
                if let Some(option) = key.strip_prefix("criteria_") {
                    // TOML has no null: criteria values are always strings.
                    if let Some(text) = value.as_str() {
                        map.insert(option.to_string(), Some(text.to_string()));
                    } else {
                        return Err(fail("criteria value must be a string"));
                    }
                }
            }
            if map.is_empty() {
                return Err(fail("choice needs options or criteria_*"));
            }
            Criteria::Map(map)
        }
        QuestionType::Noul => {
            let yes = table.get("criteria_true").and_then(|v| v.as_str()).ok_or_else(|| fail("missing criteria_true"))?;
            let no = table.get("criteria_false").and_then(|v| v.as_str()).ok_or_else(|| fail("missing criteria_false"))?;
            Criteria::Map(BTreeMap::from([
                ("true".to_string(), Some(yes.to_string())),
                ("false".to_string(), Some(no.to_string())),
            ]))
        }
        QuestionType::Score => {
            let levels = table.get("levels").and_then(|v| v.as_array()).ok_or_else(|| fail("missing levels array"))?;
            if levels.len() < 2 {
                return Err(fail("score needs at least two levels"));
            }
            let mut out = Vec::with_capacity(levels.len());
            for level in levels {
                out.push(level.as_str().ok_or_else(|| fail("level must be a string"))?.to_string());
            }
            Criteria::List(out)
        }
    };
    Ok(Question {
        qtype,
        instructions,
        criteria,
    })
}

/// triage.toml convention: the key is the text before the first colon.
fn option_key(option: &str) -> String {
    option.split(':').next().unwrap_or(option).trim().to_string()
}

// ---- client ------------------------------------------------------------------

/// Native System One client (rustls HTTPS).
#[derive(Clone)]
pub struct JevClient {
    http: reqwest::Client,
    key: ApiKey,
    endpoint: String,
}

impl JevClient {
    pub fn new(key: ApiKey) -> Result<Self, JevError> {
        let endpoint = std::env::var("TYPESAFE_ENDPOINT").unwrap_or_else(|_| SYSTEM_ONE_URL.to_string());
        let http = reqwest::Client::builder()
            .timeout(JEV_TIMEOUT)
            .build()
            .map_err(|e| JevError::Transport(e.to_string()))?;
        Ok(JevClient { http, key, endpoint })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// POST state + questions; return the raw answers map. Fail-closed
    /// like `gates/client.py::evaluate`.
    pub async fn evaluate(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, serde_json::Value>,
    ) -> Result<BTreeMap<String, serde_json::Value>, JevError> {
        let response = self
            .http
            .post(&self.endpoint)
            .header("authorization", format!("Bearer {}", self.key.expose()))
            .json(&serde_json::json!({
                "model": JEV_MODEL,
                "state": state,
                "questions": questions,
            }))
            .send()
            .await
            .map_err(|e| JevError::Transport(e.to_string()))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(JevError::Status(response.status().as_u16()));
        }
        let parsed: serde_json::Value = response
            .json()
            .await
            .map_err(|_| JevError::Protocol("malformed JSON".to_string()))?;
        let answers = parsed.get("answers").ok_or_else(|| JevError::Protocol("missing answers".to_string()))?;
        let map = answers.as_object().ok_or_else(|| JevError::Protocol("answers is not an object".to_string()))?;
        if map.is_empty() {
            return Err(JevError::Protocol("missing answers".to_string()));
        }
        Ok(map.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
    }
}

// ---- answers -----------------------------------------------------------------

pub struct ChoiceAnswer {
    pub choice: String,
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: f64,
}

fn number(value: Option<&serde_json::Value>) -> Result<f64, JevError> {
    value
        .and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|v| v as f64)))
        .ok_or_else(|| JevError::Protocol("answer is missing a number".to_string()))
}

pub fn parse_choice(answer: &serde_json::Value) -> Result<ChoiceAnswer, JevError> {
    let bad = || JevError::Protocol("invalid choice answer".to_string());
    let choice = answer.get("choice").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string();
    let probabilities = answer
        .get("probabilities")
        .and_then(|v| v.as_object())
        .ok_or_else(bad)?
        .iter()
        .map(|(key, value)| value.as_f64().map(|p| (key.clone(), p)))
        .collect::<Option<BTreeMap<_, _>>>()
        .ok_or_else(bad)?;
    Ok(ChoiceAnswer {
        choice,
        probabilities,
        confidence: number(answer.get("confidence"))?,
    })
}

pub fn parse_noul(answer: &serde_json::Value) -> Result<f64, JevError> {
    number(answer.get("noul")).map_err(|_| JevError::Protocol("invalid noul answer".to_string()))
}

pub fn parse_score(answer: &serde_json::Value) -> Result<(f64, f64), JevError> {
    Ok((
        number(answer.get("score")).map_err(|_| JevError::Protocol("invalid score answer".to_string()))?,
        number(answer.get("confidence"))?,
    ))
}

// ---- fabricated path -----------------------------------------------------------

/// Normalize for quote matching: collapse whitespace, fold curly quotes
/// (mirrors the citation-check cookbook).
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for char in text.chars() {
        let folded = match char {
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            _ => char,
        };
        if folded.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(folded);
        }
    }
    out
}

pub fn quote_in_section(section: &str, quote: &str) -> bool {
    normalize(section).contains(&normalize(quote))
}

// ---- screen routing --------------------------------------------------------------

/// Uniform strict-policy routing (hazards are config-defined, so no
/// per-hazard actions): action-threshold → `block`, review-threshold →
/// `review`, else `pass`; a high severity upgrades review to block.
pub fn route_screen(nouls: &BTreeMap<String, f64>, severity: f64) -> &'static str {
    let mut action = "pass";
    for probability in nouls.values() {
        if *probability >= SCREEN_ACTION_AT {
            action = "block";
            break;
        } else if *probability >= SCREEN_REVIEW_AT {
            action = "review";
        }
    }
    if severity >= SCREEN_SEVERITY_BLOCK && action == "review" {
        action = "block";
    }
    action
}

// ---- seat tools ------------------------------------------------------------------

/// Shared tool state. `jev` is `None` whenever the key, the endpoint, or
/// the questions are unavailable — tools then answer `not configured`
/// instead of failing the run.
#[derive(Clone)]
pub struct SeatTools {
    pub jev: Option<JevHandle>,
    pub skill_roots: Vec<String>,
}

/// Loaded questions plus an authenticated client.
#[derive(Clone)]
pub struct JevHandle {
    pub client: JevClient,
    pub questions: QuestionSet,
}

impl JevHandle {
    pub fn load(dir: &Path) -> Result<Self, JevError> {
        Ok(JevHandle {
            questions: load_questions(dir)?,
            client: JevClient::new(ApiKey::load()?)?,
        })
    }

    /// Ask one configured question.
    pub async fn ask(
        &self,
        id: &str,
        state: &serde_json::Value,
    ) -> Result<serde_json::Value, JevError> {
        let question = self
            .questions
            .questions
            .get(id)
            .ok_or_else(|| JevError::Config(format!("question `{id}` not found")))?;
        let mut questions = BTreeMap::new();
        questions.insert(id.to_string(), question.to_api_json());
        self.ask_many(&questions, state)
            .await?
            .remove(id)
            .ok_or_else(|| JevError::Protocol(format!("no answer for `{id}`")))
    }

    /// Ask a pre-built map of questions in one request (fan-out).
    pub async fn ask_many(
        &self,
        questions: &BTreeMap<String, serde_json::Value>,
        state: &serde_json::Value,
    ) -> Result<BTreeMap<String, serde_json::Value>, JevError> {
        self.client.evaluate(state, questions).await
    }
}

/// One offered tool for the pruning choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneCandidate {
    pub name: String,
    pub description: String,
}

/// Question id holding the select_tools Noul template.
pub const SELECT_TOOLS_CHECK: &str = "select_tools";

/// Drop band: relevancy below this is pruned; the band up to 0.5 is
/// uncertain and kept (fail-open toward declaring).
pub const PRUNE_BELOW: f64 = 0.4;

/// Build one Noul per candidate from the `select_tools` template.
/// The caller carries the task in the shared state.
pub fn render_select_questions(
    set: &QuestionSet,
    candidates: &[PruneCandidate],
) -> Result<BTreeMap<String, serde_json::Value>, JevError> {
    let template = set
        .questions
        .get(SELECT_TOOLS_CHECK)
        .ok_or_else(|| JevError::Config(format!("question `{SELECT_TOOLS_CHECK}` not found")))?;
    if template.qtype != QuestionType::Noul {
        return Err(JevError::Config(format!(
            "question `{SELECT_TOOLS_CHECK}` must be a noul"
        )));
    }
    let mut questions = BTreeMap::new();
    for candidate in candidates {
        let instructions = template
            .instructions
            .replace("{name}", &candidate.name)
            .replace("{description}", &candidate.description);
        let mut question = template.clone();
        question.instructions = instructions;
        questions.insert(format!("tool:{}", candidate.name), question.to_api_json());
    }
    Ok(questions)
}

/// Classify-then-act tool routing: one Noul per candidate in a single
/// request; returns the kept names plus the floor probability, or None
/// when anything is missing or fails (fail-open: declare all).
pub async fn select_tools(
    jev: &JevHandle,
    task_summary: &str,
    candidates: &[PruneCandidate],
) -> Option<(Vec<String>, f64)> {
    if candidates.is_empty() {
        return None;
    }
    let questions = render_select_questions(&jev.questions, candidates).ok()?;
    let state = serde_json::json!({"task": task_summary});
    let answers = jev.ask_many(&questions, &state).await.ok()?;
    Some(prune_decision(candidates, &answers))
}

/// Decide the kept set from Noul answers: drop only clear irrelevance
/// (`p < PRUNE_BELOW`); anything uncertain or erroring stays declared.
pub fn prune_decision(
    candidates: &[PruneCandidate],
    answers: &BTreeMap<String, serde_json::Value>,
) -> (Vec<String>, f64) {
    let mut kept = Vec::new();
    let mut floor = 1.0f64;
    for candidate in candidates {
        let key = format!("tool:{}", candidate.name);
        match answers.get(&key).and_then(|a| parse_noul(a).ok()) {
            Some(p) if p < PRUNE_BELOW => {}
            Some(p) => {
                floor = floor.min(p);
                kept.push(candidate.name.clone());
            }
            None => kept.push(candidate.name.clone()),
        }
    }
    (kept, floor)
}

fn stub(tool: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({"error": format!("tool {tool} is not configured: {reason}")})
}

fn str_arg(args: &serde_json::Value, name: &str) -> Option<String> {
    args.get(name).and_then(|v| v.as_str()).map(str::to_string)
}

/// `skill_use`: load `SKILL.md` for a skill only when called. Bodies
/// never enter the context unasked (P2 catalog note carries names only).
pub async fn skill_use(tools: &SeatTools, args: &serde_json::Value) -> serde_json::Value {
    if tools.skill_roots.is_empty() {
        return stub(TOOL_SKILL_USE, "no skill_roots");
    }
    let Some(name) = str_arg(args, "skill") else {
        return serde_json::json!({"error": "skill_use needs a `skill` argument"});
    };
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return serde_json::json!({"error": format!("invalid skill name `{name}`")});
    }
    for root in &tools.skill_roots {
        let path = Path::new(root).join(&name).join("SKILL.md");
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let mut text = String::from_utf8_lossy(&bytes).into_owned();
                let truncated = text.len() > MAX_SKILL_BYTES as usize;
                if truncated {
                    let mut end = MAX_SKILL_BYTES as usize;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                let mut result = serde_json::json!({
                    "skill": name,
                    "path": path.display().to_string(),
                    "content": text,
                });
                if truncated {
                    result["truncated"] = serde_json::json!(true);
                }
                return result;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return serde_json::json!({"error": format!("cannot read skill `{name}`: {e}")});
            }
        }
    }
    serde_json::json!({"error": format!("skill `{name}` not found in {} root(s)", tools.skill_roots.len())})
}

/// Map a Choice label onto the citation verdicts.
fn verify_verdict(choice: &str) -> &str {
    match choice {
        "supports" => "verified",
        "contradicts" => "contradicted",
        _ => "unsupported",
    }
}

/// `jev_verify`: citation check. A quote absent from the section is
/// `fabricated` with no model call; otherwise one Choice question
/// decides `verified|contradicted|unsupported`.
pub async fn jev_verify(tools: &SeatTools, args: &serde_json::Value) -> serde_json::Value {
    let Some(jev) = tools.jev.as_ref() else {
        return stub(TOOL_JEV_VERIFY, "jev is disabled or has no key/questions");
    };
    let (Some(claim), Some(section)) = (str_arg(args, "claim"), str_arg(args, "section")) else {
        return serde_json::json!({"error": "jev_verify needs `claim` and `section` arguments"});
    };
    if let Some(quote) = str_arg(args, "quote") {
        if !quote_in_section(&section, &quote) {
            return serde_json::json!({"verdict": "fabricated", "confidence": serde_json::Value::Null});
        }
    }
    let check = str_arg(args, "check").unwrap_or_else(|| CHECK_VERIFY.to_string());
    let state = serde_json::json!({"claim": claim, "section": section});
    let answer = match jev.ask(&check, &state).await {
        Ok(answer) => answer,
        Err(e) => return serde_json::json!({"error": format!("jev_verify failed: {e}")}),
    };
    match parse_choice(&answer) {
        Ok(answer) => serde_json::json!({
            "verdict": verify_verdict(&answer.choice),
            "choice": answer.choice,
            "confidence": answer.confidence,
            "probabilities": answer.probabilities,
        }),
        Err(e) => serde_json::json!({"error": format!("jev_verify failed: {e}")}),
    }
}

/// `jev_screen`: guardrail battery. Default checks are every Noul plus
/// the first Score (as severity) in id order; the whole battery costs
/// one request.
pub async fn jev_screen(tools: &SeatTools, args: &serde_json::Value) -> serde_json::Value {
    let Some(jev) = tools.jev.as_ref() else {
        return stub(TOOL_JEV_SCREEN, "jev is disabled or has no key/questions");
    };
    let Some(text) = str_arg(args, "text") else {
        return serde_json::json!({"error": "jev_screen needs a `text` argument"});
    };
    let side = str_arg(args, "side").unwrap_or_else(|| "input".to_string());
    let ids: Vec<String> = match args.get("checks").and_then(|v| v.as_array()) {
        Some(list) => list
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => jev
            .questions
            .questions
            .iter()
            .filter(|(_, q)| matches!(q.qtype, QuestionType::Noul | QuestionType::Score))
            .map(|(id, _)| id.clone())
            .collect(),
    };
    if ids.is_empty() {
        return serde_json::json!({"error": "jev_screen found no noul/score questions"});
    }
    let mut questions = BTreeMap::new();
    for id in &ids {
        let Some(question) = jev.questions.questions.get(id) else {
            return serde_json::json!({"error": format!("question `{id}` not found")});
        };
        questions.insert(id.clone(), question.to_api_json());
    }
    let state = serde_json::json!({"text": text, "side": side});
    let answers = match jev.client.evaluate(&state, &questions).await {
        Ok(answers) => answers,
        Err(e) => return serde_json::json!({"error": format!("jev_screen failed: {e}")}),
    };
    let mut nouls = BTreeMap::new();
    let mut severity = 0.0;
    let mut severity_from: Option<String> = None;
    for id in &ids {
        let Some(answer) = answers.get(id) else {
            return serde_json::json!({"error": format!("no answer for `{id}`")});
        };
        let qtype = jev.questions.questions.get(id).map(|q| q.qtype);
        match qtype {
            Some(QuestionType::Score) => {
                // Every score answer is validated; the first one in id
                // order feeds severity.
                match parse_score(answer) {
                    Ok((score, _)) => {
                        if severity_from.is_none() {
                            severity = score;
                            severity_from = Some(id.clone());
                        }
                    }
                    Err(e) => {
                        return serde_json::json!({"error": format!("jev_screen failed: {e}")})
                    }
                }
            }
            _ => match parse_noul(answer) {
                Ok(p) => {
                    nouls.insert(id.clone(), p);
                }
                Err(e) => return serde_json::json!({"error": format!("jev_screen failed: {e}")}),
            },
        }
    }
    serde_json::json!({
        "action": route_screen(&nouls, severity),
        "side": side,
        "nouls": nouls,
        "severity": severity,
        "severity_from": severity_from,
    })
}

// ---- self-check and triage ---------------------------------------------------------

/// Outcome of one self-check question.
pub struct SelfCheckAnswer {
    pub passed: bool,
    pub p: f64,
}

/// Ask the `receipt_supported` Noul over the final text plus `git diff`.
/// Missing question, non-Noul shape, or any Jev error fails closed as an
/// error (the caller then skips the follow-up, keeping the result).
pub async fn self_check(jev: &JevHandle, text: &str, diff: &str) -> Result<SelfCheckAnswer, JevError> {
    let question = jev
        .questions
        .questions
        .get(CHECK_SELF)
        .ok_or_else(|| JevError::Config(format!("question `{CHECK_SELF}` not found")))?;
    if question.qtype != QuestionType::Noul {
        return Err(JevError::Config(format!(
            "question `{CHECK_SELF}` must be a noul"
        )));
    }
    let answer = jev
        .ask(
            CHECK_SELF,
            &serde_json::json!({"text": text, "diff": diff}),
        )
        .await?;
    let p = parse_noul(&answer)?;
    Ok(SelfCheckAnswer {
        passed: p >= threshold_for(&jev.questions, CHECK_SELF, SELF_CHECK_PASS_AT),
        p,
    })
}

/// Residue triage for the Unknown kind: one Choice over `{status, text}`.
/// Returns the winning label and its probability.
pub async fn triage(jev: &JevHandle, status: &str, text: &str) -> Result<(String, f64), JevError> {
    let clipped: String = text.chars().take(2000).collect();
    let answer = jev
        .ask(
            CHECK_TRIAGE,
            &serde_json::json!({"status": status, "text": clipped}),
        )
        .await?;
    let answer = parse_choice(&answer)?;
    let p = answer.probabilities.get(&answer.choice).copied().unwrap_or(0.0);
    Ok((answer.choice, p))
}

/// Best-effort `git -C cwd diff`, capped. Empty when git is absent, the
/// dir is not a repo, or anything else goes wrong.
pub async fn git_diff(cwd: &str) -> String {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("diff")
        .arg("--")
        .arg(".")
        .output()
        .await;
    let Ok(output) = output else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.chars().count() > MAX_DIFF_CHARS {
        text.chars().take(MAX_DIFF_CHARS).collect()
    } else {
        text
    }
}

// ---- registration ---------------------------------------------------------------------

use cursor_sdk::{CustomTool, ToolCall};

fn schema(required: &[&str], properties: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

/// Register `skill_use`, `jev_verify`, and `jev_screen` on the client.
/// Must run before the agent is created so declarations reach its
/// options. Always registered (unavailable tools answer `not
/// configured`); failures are ignored — tools never fail a run.
pub async fn register_tools(
    client: &cursor_sdk::Client,
    tools: Arc<SeatTools>,
    keep: Option<&std::collections::HashSet<String>>,
) {
    let declared = |name: &str| keep.map_or(true, |set| set.contains(name));
    // Pruned tools skip registration instead of declaring a stub.
    let skill_tools = Arc::clone(&tools);
    if declared(TOOL_SKILL_USE) {
        let _ = client
            .register_tool(
                CustomTool::new(
                    TOOL_SKILL_USE,
                    TOOL_SKILL_USE_DESC,
                    schema(
                        &["skill"],
                        serde_json::json!({"skill": {
                            "type": "string",
                            "description": "Skill name (directory under a skill root)",
                        }}),
                    ),
                ),
                move |call: ToolCall| {
                    let tools = Arc::clone(&skill_tools);
                    async move { Ok(skill_use(&tools, &call.args).await) }
                },
            )
            .await;
    }

    let verify_tools = Arc::clone(&tools);
    if declared(TOOL_JEV_VERIFY) {
        let _ = client
            .register_tool(
                CustomTool::new(
                    TOOL_JEV_VERIFY,
                    TOOL_JEV_VERIFY_DESC,
                    schema(
                        &["claim", "section"],
                        serde_json::json!({
                            "claim": {"type": "string"},
                            "section": {"type": "string", "description": "Source text the claim rests on"},
                            "quote": {"type": "string", "description": "Optional verbatim quote; absent quotes are fabricated"},
                            "check": {"type": "string", "description": "Question id (default relation)"},
                        }),
                    ),
                ),
                move |call: ToolCall| {
                    let tools = Arc::clone(&verify_tools);
                    async move { Ok(jev_verify(&tools, &call.args).await) }
                },
            )
            .await;
    }

    let screen_tools = Arc::clone(&tools);
    if declared(TOOL_JEV_SCREEN) {
        let _ = client
            .register_tool(
                CustomTool::new(
                    TOOL_JEV_SCREEN,
                    TOOL_JEV_SCREEN_DESC,
                    schema(
                        &["text"],
                    serde_json::json!({
                        "text": {"type": "string"},
                        "side": {"type": "string", "description": "input or output (default input)"},
                        "checks": {"type": "array", "items": {"type": "string"}, "description": "Question ids (default every noul plus the first score)"},
                    }),
                    ),
                ),
                move |call: ToolCall| {
                    let tools = Arc::clone(&screen_tools);
                    async move { Ok(jev_screen(&tools, &call.args).await) }
                },
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_toml(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn sample_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seat-jev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_toml(
            &dir,
            "a.toml",
            "[question.relation]\n\
             type = \"choice\"\n\
             instructions = \"How does the section relate to the claim?\"\n\
             criteria_supports = \"states it\"\n\
             criteria_contradicts = \"opposite\"\n\
             criteria_says_nothing = \"silent\"\n\
             \n\
             [question.is_urgent]\n\
             type = \"noul\"\n\
             instructions = \"Urgent?\"\n\
             criteria_true = \"time-sensitive\"\n\
             criteria_false = \"routine\"\n",
        );
        write_toml(
            &dir,
            "b.toml",
            "[question.severity]\n\
             type = \"score\"\n\
             instructions = \"Harm?\"\n\
             levels = [\"none\", \"mild\", \"serious\", \"severe\"]\n\
             \n\
             [thresholds]\n\
             is_urgent_at = 0.8\n",
        );
        dir
    }

    #[test]
    fn questions_load_from_toml_files() {
        let dir = sample_dir();
        let set = load_questions(&dir).unwrap();
        assert_eq!(set.questions.len(), 3);
        let relation = &set.questions["relation"];
        assert_eq!(relation.qtype, QuestionType::Choice);
        let api = relation.to_api_json();
        assert_eq!(api["criteria"]["supports"], serde_json::json!("states it"));
        assert_eq!(set.thresholds["is_urgent_at"], 0.8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_array_uses_key_before_colon() {
        let dir = std::env::temp_dir().join(format!("seat-jev-opt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_toml(
            &dir,
            "t.toml",
            "[question.failure_class]\n\
             type = \"choice\"\n\
             instructions = \"What class?\"\n\
             options = [\"code: wrong impl.\", \"flake: transient.\"]\n",
        );
        let set = load_questions(&dir).unwrap();
        let api = set.questions["failure_class"].to_api_json();
        // Values keep the full option text (triage.py convention).
        assert_eq!(api["criteria"]["code"], serde_json::json!("code: wrong impl."));
        assert_eq!(api["criteria"]["flake"], serde_json::json!("flake: transient."));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_questions_load_with_expected_ids() {
        // Integration tests run with cwd = the package dir.
        let set = load_questions(Path::new("questions")).unwrap();
        assert_eq!(set.questions["relation"].qtype, QuestionType::Choice);
        assert_eq!(set.questions["triage"].qtype, QuestionType::Choice);
        assert_eq!(set.questions["receipt_supported"].qtype, QuestionType::Noul);
        assert_eq!(set.questions["severity"].qtype, QuestionType::Score);
        assert_eq!(set.questions["select_tools"].qtype, QuestionType::Noul);
    }

    #[test]
    fn select_template_renders_per_tool() {
        let dir = std::env::temp_dir().join(format!("seat-jev-sel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tools.toml"),
            "[question.select_tools]\n\
             type = \"noul\"\n\
             instructions = \"Is `{name}` relevant? {description}\"\n\
             criteria_true = \"yes\"\n\
             criteria_false = \"no\"\n",
        )
        .unwrap();
        let set = load_questions(&dir).unwrap();
        let candidates = vec![
            PruneCandidate {
                name: "a".into(),
                description: "does A".into(),
            },
            PruneCandidate {
                name: "b".into(),
                description: "does B".into(),
            },
        ];
        let rendered = render_select_questions(&set, &candidates).unwrap();
        assert_eq!(rendered.len(), 2);
        assert!(rendered["tool:a"]["instructions"]
            .as_str()
            .unwrap()
            .contains("`a`"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_decision_drops_only_clear_irrelevance() {
        let candidates = vec![
            PruneCandidate { name: "keep".into(), description: String::new() },
            PruneCandidate { name: "drop".into(), description: String::new() },
            PruneCandidate { name: "shy".into(), description: String::new() },
            PruneCandidate { name: "mute".into(), description: String::new() },
        ];
        let answers = BTreeMap::from([
            ("tool:keep".to_string(), serde_json::json!({"type": "noul", "noul": 0.9})),
            ("tool:drop".to_string(), serde_json::json!({"type": "noul", "noul": 0.1})),
            ("tool:shy".to_string(), serde_json::json!({"type": "noul", "noul": 0.45})),
        ]);
        // mute has no answer: kept (fail-open). shy is uncertain: kept.
        let (kept, floor) = prune_decision(&candidates, &answers);
        assert_eq!(kept, vec!["keep".to_string(), "shy".to_string(), "mute".to_string()]);
        assert!((floor - 0.45).abs() < 1e-9);
    }

    #[test]
    fn thresholds_come_from_toml_with_fallback() {
        let dir = std::env::temp_dir().join(format!("seat-jev-thr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("q.toml"),
            "[question.receipt_supported]\n\
             type = \"noul\"\n\
             instructions = \"x\"\n\
             criteria_true = \"y\"\n\
             criteria_false = \"n\"\n\
             \n\
             [thresholds]\n\
             receipt_supported_at = 0.8\n",
        )
        .unwrap();
        let set = load_questions(&dir).unwrap();
        assert_eq!(threshold_for(&set, "receipt_supported", 0.5), 0.8);
        assert_eq!(threshold_for(&set, "missing", 0.5), 0.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_configs_fail() {
        let dir = std::env::temp_dir().join(format!("seat-jev-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Duplicate id across files.
        write_toml(&dir, "1.toml", "[question.a]\ntype = \"noul\"\ninstructions = \"x\"\ncriteria_true = \"y\"\ncriteria_false = \"n\"\n");
        write_toml(&dir, "2.toml", "[question.a]\ntype = \"noul\"\ninstructions = \"x\"\ncriteria_true = \"y\"\ncriteria_false = \"n\"\n");
        assert!(load_questions(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        // Unknown top-level table.
        std::fs::create_dir_all(&dir).unwrap();
        write_toml(&dir, "1.toml", "[bogus]\nx = 1\n");
        assert!(load_questions(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        // Empty dir.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_questions(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_file_parsing_mirrors_keys_py() {
        let dir = std::env::temp_dir().join(format!("seat-jev-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("typesafe.env");
        std::fs::write(
            &path,
            "# comment\nexport TYPESAFE_API_KEY=\"abc123\"\nOTHER=1\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(parse_key_file(&path, "TYPESAFE_API_KEY").unwrap(), "abc123");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(matches!(
                parse_key_file(&path, "TYPESAFE_API_KEY"),
                Err(JevError::BadKeyMode(_))
            ));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_key_never_prints() {
        let key = ApiKey("super-secret".to_string());
        assert_eq!(format!("{key:?}"), "ApiKey([redacted])");
    }

    #[test]
    fn normalize_and_fabricated_path() {
        assert!(quote_in_section("the  quick\nbrown", "the quick brown"));
        assert!(quote_in_section("say “hi”", "say \"hi\""));
        assert!(!quote_in_section("nothing here", "absent quote"));
    }

    #[test]
    fn screen_routing_thresholds() {
        assert_eq!(route_screen(&BTreeMap::from([("a".to_string(), 0.1)]), 0.0), "pass");
        assert_eq!(route_screen(&BTreeMap::from([("a".to_string(), 0.5)]), 0.0), "review");
        assert_eq!(route_screen(&BTreeMap::from([("a".to_string(), 0.9)]), 0.0), "block");
        // Severity upgrades a review, never downgrades a pass on its own.
        assert_eq!(route_screen(&BTreeMap::from([("a".to_string(), 0.5)]), 2.5), "block");
        assert_eq!(route_screen(&BTreeMap::from([("a".to_string(), 0.1)]), 2.5), "pass");
    }

    #[tokio::test]
    async fn skill_use_reads_only_on_call() {
        let dir = std::env::temp_dir().join(format!("seat-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deploy")).unwrap();
        std::fs::write(dir.join("deploy").join("SKILL.md"), "# deploy\n").unwrap();
        let tools = SeatTools {
            jev: None,
            skill_roots: vec![dir.display().to_string()],
        };
        let got = skill_use(&tools, &serde_json::json!({"skill": "deploy"})).await;
        assert_eq!(got["content"], serde_json::json!("# deploy\n"));
        let missing = skill_use(&tools, &serde_json::json!({"skill": "nope"})).await;
        assert!(missing.get("error").is_some());
        let evil = skill_use(&tools, &serde_json::json!({"skill": "../evil"})).await;
        assert!(evil.get("error").is_some());
        let bare = SeatTools {
            jev: None,
            skill_roots: vec![],
        };
        let stubbed = skill_use(&bare, &serde_json::json!({"skill": "deploy"})).await;
        assert!(stubbed["error"].as_str().unwrap().contains("not configured"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tools_without_jev_answer_not_configured() {
        let tools = SeatTools {
            jev: None,
            skill_roots: vec![],
        };
        let answer =
            jev_verify(&tools, &serde_json::json!({"claim": "c", "section": "s"})).await;
        let error = answer.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(error.contains("not configured"), "{answer}");
        let answer = jev_screen(&tools, &serde_json::json!({"text": "t"})).await;
        let error = answer.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(error.contains("not configured"), "{answer}");
    }
}
