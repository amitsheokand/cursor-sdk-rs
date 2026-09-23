//! Omission-reporting context builder, ported from unreal-agent
//! `harness/contextbuilder`.
//!
//! unreal-agent defines `Report{Changes}` but never fills it in; the seat
//! actually does: [`build_context`] assembles the prompt parts under
//! `limits.context_chars` and records every omission or clip as a
//! [`ContextChange`](crate::protocol::ContextChange). Skill bodies stay
//! deferred (P6 `skill_use` loads them on call) — only the catalog note
//! (name, description, path) enters the context, in Go's
//! `<available_skills>` XML shape.

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::clip::clip_to_budget;
use crate::protocol::{ContextChange, ContextChangeKind, PromptPart, SeatRequest};

/// One deferred skill for the catalog note: name, description, and path
/// only — never the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// Preamble for the catalog note, matching Go's `skill-preamble.md`.
pub const SKILL_PREAMBLE: &str = "The following skills provide specialized instructions for specific tasks. Use SkillUse to load a skill's file when the task matches its description. When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool calls.";

const SEPARATOR: &str = "\n\n";

/// Assembled prompt text plus the omission report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltContext {
    pub text: String,
    pub changes: Vec<ContextChange>,
}

/// Assemble the prompt parts under `request.limits.context_chars`.
///
/// Part order is task, body, steer, skill catalog. Sacrifice order when
/// over budget: drop the catalog note (bulky, recoverable via `skill_use`),
/// drop the steer, clip the body, clip the task. Lengths are rune counts.
/// The result fits the budget whenever the budget is realistic (see
/// `clip_to_budget`).
pub fn build_context(request: &SeatRequest, skills: &[SkillEntry]) -> BuiltContext {
    let budget = request.limits.context_chars;
    let mut changes = Vec::new();

    let task = format_task(&request.prompt);
    let body = request.prompt.body.as_str();

    // Droppable parts, lowest priority first.
    let mut steer: Option<&str> = request.prompt.steer.as_deref();
    let mut catalog: Option<String> = (!skills.is_empty()).then(|| format_catalog_note(skills));

    let mut total = exact_total(&[task.as_str(), body], steer, catalog.as_deref());

    if catalog.is_some() && total > budget {
        catalog = None;
        changes.push(omitted("skills.catalog", "over context_chars budget"));
        total = exact_total(&[task.as_str(), body], steer, catalog.as_deref());
    }
    if steer.is_some() && total > budget {
        steer = None;
        changes.push(omitted("prompt.steer", "over context_chars budget"));
        total = exact_total(&[task.as_str(), body], steer, catalog.as_deref());
    }

    let mut body_kept = Cow::Borrowed(body);
    if total > budget {
        // Room for the body: everything else (parts + separators) stays.
        let room = budget.saturating_sub(total - part_len(&body_kept));
        let (clipped, was) = clip_to_budget(body, room);
        if was {
            changes.push(truncated("prompt.body", "over context_chars budget"));
            body_kept = Cow::Owned(clipped);
        }
        total = exact_total(&[task.as_str(), &body_kept], steer, catalog.as_deref());
    }

    let mut task_kept = Cow::Borrowed(task.as_str());
    if total > budget {
        let others = total - part_len(&task_kept);
        let room = budget.saturating_sub(others);
        let (clipped, was) = clip_to_budget(&task_kept, room);
        if was {
            changes.push(truncated("prompt.task", "over context_chars budget"));
            task_kept = Cow::Owned(clipped);
        }
    }

    let mut text = String::with_capacity(budget.min(1_000_000));
    text.push_str(&task_kept);
    text.push_str(SEPARATOR);
    text.push_str(&body_kept);
    if let Some(steer) = steer {
        text.push_str(SEPARATOR);
        text.push_str(steer);
    }
    if let Some(catalog) = catalog.as_deref() {
        text.push_str(SEPARATOR);
        text.push_str(catalog);
    }
    BuiltContext { text, changes }
}

fn format_task(prompt: &PromptPart) -> String {
    if prompt.effort_tag.is_empty() {
        prompt.task.clone()
    } else {
        format!("[{}] {}", prompt.effort_tag, prompt.task)
    }
}

/// Catalog note: preamble plus `<available_skills>` XML with
/// name/description/location per skill, matching Go's
/// `formatSkillsForPrompt`.
pub fn format_catalog_note(skills: &[SkillEntry]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(
        SKILL_PREAMBLE.len() + skills.len() * 128,
    );
    out.push_str(SKILL_PREAMBLE);
    out.push_str("\n\n<available_skills>");
    for skill in skills {
        let _ = write!(
            out,
            "<skill><name>{}</name><description>{}</description><location>{}</location></skill>",
            xml_escape(&skill.name),
            xml_escape(&skill.description),
            xml_escape(&skill.path),
        );
    }
    out.push_str("</available_skills>");
    out
}

fn xml_escape(text: &str) -> Cow<'_, str> {
    if !text.contains(['<', '>', '&', '\'', '"']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for char in text.chars() {
        match char {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(char),
        }
    }
    Cow::Owned(out)
}

fn part_len(text: &str) -> usize {
    text.chars().count()
}

/// Exact joined length of the surviving parts (task, body always
/// present; separators only between present parts).
fn exact_total(essential: &[&str], steer: Option<&str>, catalog: Option<&str>) -> usize {
    let mut total = 0;
    let mut first = true;
    let mut push = |text: &str| {
        if !first {
            total += SEPARATOR.chars().count();
        }
        first = false;
        total += part_len(text);
    };
    for part in essential {
        push(part);
    }
    if let Some(steer) = steer {
        push(steer);
    }
    if let Some(catalog) = catalog {
        push(catalog);
    }
    total
}

fn omitted(source: &str, reason: &str) -> ContextChange {
    ContextChange {
        kind: ContextChangeKind::Omitted,
        source: source.to_string(),
        reason: reason.to_string(),
    }
}

fn truncated(source: &str, reason: &str) -> ContextChange {
    ContextChange {
        kind: ContextChangeKind::Truncated,
        source: source.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Limits, ModelParams, ModelRef};

    fn request(task: &str, body: &str, steer: Option<&str>, budget: usize) -> SeatRequest {
        SeatRequest {
            v: 1,
            request_id: "pkt:1".to_string(),
            cwd: "/repo".to_string(),
            model: ModelRef {
                id: "composer-2.5".to_string(),
                params: ModelParams {
                    effort: Some("high".to_string()),
                    extra: Default::default(),
                },
            },
            prompt: PromptPart {
                task: task.to_string(),
                effort_tag: "high".to_string(),
                body: body.to_string(),
                steer: steer.map(str::to_string),
            },
            mcp_servers: vec![],
            disallowed_tools: vec!["task".to_string()],
            tools_enabled: vec![],
            skill_roots: vec![],
            jev: Default::default(),
            limits: Limits {
                context_chars: budget,
                clip_chars: 40_000,
                timeout_s: 600,
                heartbeat_s: 30,
            },
            session_dir: None,
        }
    }

    fn skill(name: &str) -> SkillEntry {
        SkillEntry {
            name: name.to_string(),
            description: format!("does {name}"),
            path: format!("/skills/{name}/SKILL.md"),
        }
    }

    #[test]
    fn everything_fits_means_no_changes() {
        let req = request("do it", "the body", Some("nudge"), 100_000);
        let built = build_context(&req, &[skill("a"), skill("b")]);
        assert!(built.changes.is_empty());
        assert!(built.text.contains("[high] do it"));
        assert!(built.text.contains("the body"));
        assert!(built.text.contains("nudge"));
        assert!(built.text.contains("<available_skills>"));
        assert!(built.text.contains("<name>a</name>"));
        assert!(built.text.contains("<location>/skills/b/SKILL.md</location>"));
    }

    #[test]
    fn body_over_budget_is_clipped_and_reported() {
        let req = request("do it", &"b".repeat(10_000), None, 1_000);
        let built = build_context(&req, &[]);
        assert_eq!(
            built.changes,
            vec![truncated("prompt.body", "over context_chars budget")]
        );
        assert!(built.text.contains("bytes truncated"));
        assert!(built.text.chars().count() <= 1_000);
    }

    #[test]
    fn steer_and_catalog_drop_before_body_clips() {
        let body = "b".repeat(500);
        let req = request("do it", &body, Some("nudge"), 700);
        let built = build_context(&req, &[skill("a")]);
        // Catalog note alone exceeds the slack, so it drops; steer survives.
        assert!(
            built
                .changes
                .contains(&omitted("skills.catalog", "over context_chars budget"))
        );
        assert!(!built.text.contains("<available_skills>"));
        assert!(built.text.contains("nudge"));
        assert!(built.text.chars().count() <= 700);
    }

    #[test]
    fn tight_budget_omits_steer_first() {
        let req = request("do it", "body", Some("a much longer steer message"), 40);
        let built = build_context(&req, &[]);
        assert!(
            built
                .changes
                .iter()
                .any(|change| change.source == "prompt.steer")
        );
        assert!(!built.text.contains("steer message"));
        assert!(built.text.chars().count() <= 40);
    }

    #[test]
    fn catalog_note_carries_names_paths_and_no_bodies() {
        let note = format_catalog_note(&[skill("deploy")]);
        assert!(note.starts_with(SKILL_PREAMBLE));
        assert!(note.contains("<name>deploy</name>"));
        assert!(note.contains("<description>does deploy</description>"));
        assert!(note.contains("<location>/skills/deploy/SKILL.md</location>"));
    }

    #[test]
    fn catalog_note_escapes_xml() {
        let note = format_catalog_note(&[SkillEntry {
            name: "a<b".to_string(),
            description: "x & y".to_string(),
            path: "/s".to_string(),
        }]);
        assert!(note.contains("<name>a&lt;b</name>"));
        assert!(note.contains("x &amp; y"));
    }
}
