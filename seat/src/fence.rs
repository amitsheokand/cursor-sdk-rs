//! Worktree fence: escape detection, protected-root snapshots, git drift.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use cursor_sdk::StreamMessage;
use serde_json::Value;

/// Metadata recorded for one file in a protected-root snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub content_hash: u64,
}

/// Path → metadata digest for fence entries under a protected root.
pub type Digest = BTreeMap<PathBuf, FileMeta>;

/// Whether `rel_path` (worktree-relative, normalized) lies inside the fence.
pub fn in_fence(rel_path: &str, cwd: &Path, fence: &[String]) -> bool {
    if fence.is_empty() {
        return true;
    }
    let entries = worktree_fence_entries(cwd, fence);
    if entries.is_empty() {
        return true;
    }
    let rel = normalize_rel(rel_path);
    if rel.is_empty() {
        return false;
    }
    entries.iter().any(|entry| entry_matches(&rel, entry))
}

/// Fence entries that apply inside `cwd` (worktree-relative, normalized).
pub fn worktree_fence_entries(cwd: &Path, fence: &[String]) -> Vec<String> {
    let cwd_canon = resolved_clean(cwd);
    let mut out = Vec::new();
    for raw in fence {
        if let Some(rel) = entry_relative_to_cwd(raw, &cwd_canon) {
            out.push(rel);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn fence_path_token(raw: &str) -> &str {
    raw.trim().split_whitespace().next().unwrap_or("")
}

fn expand_tilde(token: &str) -> PathBuf {
    let token = token.trim();
    if let Some(rest) = token.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(token)
}

/// Resolve one fence entry to a cwd-relative path, or skip if outside the worktree.
fn entry_relative_to_cwd(raw: &str, cwd_canon: &Path) -> Option<String> {
    let token = fence_path_token(raw);
    if token.is_empty() {
        return None;
    }
    let expanded = expand_tilde(token);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        cwd_canon.join(expanded)
    };
    let path = resolved_clean(&path);
    if !path_starts_with(&path, cwd_canon) {
        return None;
    }
    let rel = path.strip_prefix(cwd_canon).ok()?;
    let rel_str = normalize_rel(&rel.to_string_lossy());
    if rel_str.is_empty() {
        None
    } else {
        Some(rel_str)
    }
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
    let prefix = format!("{entry}/");
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

fn content_hash(path: &Path) -> u64 {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Snapshot fence entries under each `protected_root` (worktree-relative entries only).
pub fn snapshot(protected_roots: &[PathBuf], fence: &[String], cwd: &Path) -> Digest {
    let mut digest = BTreeMap::new();
    let rel_entries = worktree_fence_entries(cwd, fence);
    if rel_entries.is_empty() {
        return digest;
    }
    for root in protected_roots {
        for entry in &rel_entries {
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
                content_hash: content_hash(path),
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
    if worktree_fence_entries(cwd, fence).is_empty() {
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
        if !in_fence(rel, cwd, fence) {
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp(prefix: &str) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn in_fence_file_and_dir_prefix() {
        let cwd = unique_temp("fence-in");
        assert!(in_fence("src/foo.rs", &cwd, &["src/foo.rs".into()]));
        assert!(in_fence("src/foo.rs", &cwd, &["src".into()]));
        assert!(!in_fence("src/foo.rs", &cwd, &["src/bar".into()]));
        assert!(!in_fence("notsrc/foo", &cwd, &["src".into()]));
    }

    #[test]
    fn tilde_fence_entry_under_cwd_matches() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let cwd = home.join(format!(".cursor-seat-fence-tilde-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&cwd);
        fs::create_dir_all(&cwd).unwrap();
        let file_rel = "marked.txt";
        fs::write(cwd.join(file_rel), b"x").unwrap();
        let rel_from_home = cwd.strip_prefix(&home).expect("cwd under HOME");
        let entry = format!("~/{}/{}", rel_from_home.to_string_lossy(), file_rel);
        assert!(in_fence(file_rel, &cwd, &[entry]));
    }

    #[test]
    fn fence_entry_strips_trailing_prose() {
        let cwd = unique_temp("fence-prose");
        fs::write(cwd.join("pre_steer.py"), b"x").unwrap();
        assert!(in_fence(
            "pre_steer.py",
            &cwd,
            &["pre_steer.py plane_allows".into()],
        ));
    }

    #[test]
    fn absolute_fence_outside_cwd_is_ignored() {
        let cwd = unique_temp("fence-outside-cwd");
        let outside = unique_temp("fence-outside-other");
        let file = outside.join("only.txt");
        fs::write(&file, b"x").unwrap();
        let entry = file.display().to_string();
        assert!(worktree_fence_entries(&cwd, &[entry]).is_empty());
    }

    #[test]
    fn fence_only_outside_entries_yields_no_drift() {
        let cwd = unique_temp("fence-drift-empty");
        init_git(&cwd);
        fs::write(cwd.join("dirty.txt"), b"x").unwrap();
        let outside = unique_temp("fence-drift-out");
        let entry = outside.join("x").display().to_string();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let paths = rt.block_on(drift(&cwd, &[entry]));
        assert!(paths.is_empty());
    }

    fn init_git(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "t@test"])
            .current_dir(dir)
            .output()
            .expect("git config");
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(dir)
            .output()
            .expect("git config");
    }

    #[test]
    fn tool_escape_blocks_edit_outside_cwd() {
        let cwd = unique_temp("fence-cwd");
        let args = serde_json::json!({"path": "/etc/passwd"})
            .as_object()
            .unwrap()
            .clone();
        let hit = tool_escape("edit", &args, &cwd, &[]);
        assert!(hit.is_some());
    }

    #[test]
    fn tool_escape_allows_read_outside() {
        let cwd = unique_temp("fence-cwd-read");
        let args = serde_json::json!({"path": "/etc/passwd"})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("read", &args, &cwd, &[]).is_none());
    }

    #[test]
    fn snapshot_detects_content_change() {
        let dir = unique_temp("fence-snap");
        let file = dir.join("tracked.txt");
        fs::write(&file, b"a").unwrap();
        let before = snapshot(&[dir.clone()], &["tracked.txt".into()], &dir);
        fs::write(&file, b"ab").unwrap();
        let after = snapshot(&[dir.clone()], &["tracked.txt".into()], &dir);
        let delta = changed(&before, &after);
        assert_eq!(delta, vec![file]);
    }

    #[test]
    fn snapshot_ignores_mtime_only_change() {
        let dir = unique_temp("fence-snap-mtime");
        let file = dir.join("tracked.txt");
        fs::write(&file, b"same").unwrap();
        let before = snapshot(&[dir.clone()], &["tracked.txt".into()], &dir);
        fs::write(&file, b"same").unwrap();
        std::process::Command::new("touch")
            .arg(&file)
            .status()
            .expect("touch");
        let after = snapshot(&[dir.clone()], &["tracked.txt".into()], &dir);
        assert!(changed(&before, &after).is_empty());
    }
}
