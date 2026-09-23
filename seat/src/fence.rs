//! Worktree fence: escape detection, protected-root snapshots, git drift.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use cursor_sdk::StreamMessage;
use serde_json::Value;

/// Metadata recorded for one file in a protected-root snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub modified: SystemTime,
}

/// Path → metadata digest for fence entries under a protected root.
pub type Digest = BTreeMap<PathBuf, FileMeta>;

/// Whether `rel_path` (worktree-relative, normalized) lies inside the fence.
pub fn in_fence(rel_path: &str, fence: &[String]) -> bool {
    if fence.is_empty() {
        return true;
    }
    let rel = normalize_rel(rel_path);
    if rel.is_empty() {
        return false;
    }
    fence.iter().any(|entry| entry_matches(&rel, entry))
}

fn normalize_rel(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches("./");
    let p = Path::new(trimmed);
    let mut parts = Vec::new();
    for component in p.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    parts.join("/")
}

fn entry_matches(rel: &str, entry: &str) -> bool {
    let entry = normalize_rel(entry);
    if entry.is_empty() {
        return false;
    }
    if rel == entry {
        return true;
    }
    let prefix = format!("{}/", entry);
    rel.starts_with(&prefix)
}

/// Writing `tool_name` with a path-like arg that resolves outside `cwd` (and
/// outside `allowed_extra`, e.g. `session_dir`). Read-only tools never escape.
pub fn tool_escape(
    tool_name: &str,
    args: &serde_json::Map<String, Value>,
    cwd: &Path,
    allowed_extra: &[PathBuf],
) -> Option<PathBuf> {
    let name = tool_name.trim().to_lowercase();
    if is_read_only(&name) {
        return None;
    }
    if name == "shell" || name.ends_with("shell") {
        return shell_escape(args, cwd, allowed_extra);
    }
    if !is_writing_tool(&name) {
        return None;
    }
    let path = path_from_args(args)?;
    resolve_escape(&path, cwd, allowed_extra)
}

fn is_read_only(name: &str) -> bool {
    matches!(
        name,
        "read" | "grep" | "glob" | "glob_file_search" | "list" | "list_dir" | "rg"
    ) || name.contains("read") && !name.contains("thread")
}

fn is_writing_tool(name: &str) -> bool {
    matches!(
        name,
        "edit"
            | "write"
            | "delete"
            | "strreplace"
            | "apply_patch"
            | "applypatch"
            | "search_replace"
    ) || name.contains("edit")
        || name.contains("write")
        || name.contains("delete")
        || name.contains("patch")
}

fn path_from_args(args: &serde_json::Map<String, Value>) -> Option<String> {
    const KEYS: &[&str] = &[
        "path",
        "file_path",
        "filePath",
        "target_file",
        "targetFile",
        "target_directory",
        "targetDirectory",
    ];
    for key in KEYS {
        if let Some(value) = args.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn shell_escape(
    args: &serde_json::Map<String, Value>,
    cwd: &Path,
    allowed_extra: &[PathBuf],
) -> Option<PathBuf> {
    const KEYS: &[&str] = &["cwd", "working_directory", "workingDirectory"];
    for key in KEYS {
        if let Some(value) = args.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return resolve_escape(trimmed, cwd, allowed_extra);
            }
        }
    }
    None
}

fn resolve_escape(
    path: &str,
    cwd: &Path,
    allowed_extra: &[PathBuf],
) -> Option<PathBuf> {
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let resolved = resolved_clean(&resolved);
    if path_allowed(&resolved, cwd, allowed_extra) {
        None
    } else {
        Some(resolved)
    }
}

fn resolved_clean(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_allowed(path: &Path, cwd: &Path, allowed_extra: &[PathBuf]) -> bool {
    let cwd = resolved_clean(cwd);
    let path = resolved_clean(path);
    if path_starts_with(&path, &cwd) {
        return true;
    }
    allowed_extra
        .iter()
        .any(|extra| path_starts_with(&path, &resolved_clean(extra)))
}

fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    path.components().zip(prefix.components()).all(|(a, b)| a == b)
        && path.components().count() >= prefix.components().count()
}

/// Snapshot fence entries under each `protected_root`.
pub fn snapshot(protected_roots: &[PathBuf], fence: &[String]) -> Digest {
    let mut digest = BTreeMap::new();
    if fence.is_empty() {
        return digest;
    }
    for root in protected_roots {
        for entry in fence {
            let target = root.join(entry);
            if target.is_file() {
                record_file(&mut digest, &target);
            } else if target.is_dir() {
                walk_dir(&mut digest, &target);
            }
        }
    }
    digest
}

fn record_file(digest: &mut Digest, path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        digest.insert(
            path.to_path_buf(),
            FileMeta {
                size: meta.len(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            },
        );
    }
}

fn walk_dir(digest: &mut Digest, dir: &Path) {
    let entries = std::fs::read_dir(dir);
    let Ok(entries) = entries else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            record_file(digest, &path);
        } else if path.is_dir() {
            walk_dir(digest, &path);
        }
    }
}

/// Paths whose metadata changed between two snapshots.
pub fn changed(before: &Digest, after: &Digest) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (path, meta) in before {
        match after.get(path) {
            Some(next) if next == meta => {}
            _ => out.push(path.clone()),
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            out.push(path.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Git porcelain paths under `cwd` that are not inside the fence.
pub async fn drift(cwd: &Path, fence: &[String]) -> Vec<String> {
    if fence.is_empty() {
        return Vec::new();
    }
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("status")
        .arg("--porcelain")
        .arg("--untracked-files=all")
        .output()
        .await;
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in text.lines() {
        if line.len() < 4 {
            continue;
        }
        let rel = line[3..].trim();
        if rel.is_empty() {
            continue;
        }
        // status may list "a -> b" for renames; take the destination.
        let rel = rel.rsplit(" -> ").next().unwrap_or(rel);
        if !in_fence(rel, fence) {
            paths.push(rel.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Args map for a stream `tool_call`, same extraction as [`crate::run::tool_label`].
pub fn tool_args_from_message(message: &StreamMessage) -> (String, serde_json::Map<String, Value>) {
    let name = message
        .payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = message
        .payload
        .get("args")
        .or_else(|| message.payload.get("message")?.get("args"));
    let mut mapping = serde_json::Map::new();
    if let Some(Value::Object(map)) = args {
        mapping = map.clone();
    } else if let Some(text) = args.and_then(Value::as_str) {
        if text.trim_start().starts_with('{') {
            if let Ok(Value::Object(map)) = serde_json::from_str(text) {
                mapping = map;
            }
        }
    }
    (name, mapping)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn in_fence_file_and_dir_prefix() {
        assert!(in_fence("src/foo.rs", &["src/foo.rs".into()]));
        assert!(in_fence("src/foo.rs", &["src".into()]));
        assert!(!in_fence("src/foo.rs", &["src/bar".into()]));
        assert!(!in_fence("notsrc/foo", &["src".into()]));
    }

    #[test]
    fn tool_escape_blocks_edit_outside_cwd() {
        let cwd = std::env::temp_dir().join("fence-cwd");
        let _ = fs::create_dir_all(&cwd);
        let args = serde_json::json!({"path": "/etc/passwd"})
            .as_object()
            .unwrap()
            .clone();
        let hit = tool_escape("edit", &args, &cwd, &[]);
        assert!(hit.is_some());
    }

    #[test]
    fn tool_escape_allows_read_outside() {
        let cwd = std::env::temp_dir().join("fence-cwd-read");
        let _ = fs::create_dir_all(&cwd);
        let args = serde_json::json!({"path": "/etc/passwd"})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("read", &args, &cwd, &[]).is_none());
    }

    #[test]
    fn snapshot_detects_change() {
        let dir = std::env::temp_dir().join("fence-snap");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tracked.txt");
        fs::write(&file, b"a").unwrap();
        let before = snapshot(&[dir.clone()], &["tracked.txt".into()]);
        fs::write(&file, b"ab").unwrap();
        let after = snapshot(&[dir.clone()], &["tracked.txt".into()]);
        let delta = changed(&before, &after);
        assert_eq!(delta, vec![file]);
    }
}
