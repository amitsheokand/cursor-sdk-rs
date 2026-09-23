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
    let path = resolve_fence_path(token, cwd_canon);
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
    for path in all_path_from_args(args) {
        if let Some(hit) = resolve_escape(&path, cwd, allowed_extra) {
            return Some(hit);
        }
    }
    None
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

const PATH_ARG_KEYS: &[&str] = &[
    "path",
    "file_path",
    "filePath",
    "target_file",
    "targetFile",
    "target_directory",
    "targetDirectory",
];

fn collect_path_args(map: &serde_json::Map<String, Value>, out: &mut Vec<String>) {
    for key in PATH_ARG_KEYS {
        if let Some(value) = map.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
}

fn all_path_from_args(args: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut paths = Vec::new();
    collect_path_args(args, &mut paths);
    if let Some(Value::Object(nested)) = args.get("arguments") {
        collect_path_args(nested, &mut paths);
    }
    paths
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
                if let Some(hit) = resolve_escape(trimmed, cwd, allowed_extra) {
                    return Some(hit);
                }
            }
        }
    }
    None
}

fn resolve_escape(path: &str, cwd: &Path, allowed_extra: &[PathBuf]) -> Option<PathBuf> {
    let resolved = resolve_fence_path(path, cwd);
    if path_allowed(&resolved, cwd, allowed_extra) {
        None
    } else {
        Some(resolved)
    }
}

fn join_base(raw: &str, base: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.starts_with("~/") || trimmed == "~" {
        expand_tilde(trimmed)
    } else if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        base.join(trimmed)
    }
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => prefix.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop();
            }
            Component::Normal(name) => stack.push(name.to_os_string()),
        }
    }
    for part in stack {
        prefix.push(part);
    }
    prefix
}

fn append_lexical(mut base: PathBuf, comps: &[Component]) -> PathBuf {
    for comp in comps {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            Component::Normal(name) => base.push(name),
            Component::Prefix(_) | Component::RootDir => base.push(comp.as_os_str()),
        }
    }
    base
}

fn canonicalize_component(path: &Path) -> PathBuf {
    if path
        .symlink_metadata()
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        if let Ok(link) = std::fs::read_link(path) {
            let resolved = if link.is_absolute() {
                lexically_normalize(&link)
            } else {
                lexically_normalize(
                    &path
                        .parent()
                        .unwrap_or(Path::new("/"))
                        .join(link),
                )
            };
            if resolved.exists() {
                return std::fs::canonicalize(&resolved).unwrap_or(resolved);
            }
            return resolved;
        }
    }
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Component-wise resolution: canonicalize each existing segment; honour `..`
/// on the resolved accumulator; lexical tail for not-yet-existing suffixes.
fn resolve_components(path: &Path) -> PathBuf {
    let comps: Vec<_> = path.components().collect();
    let mut i = 0;
    let mut acc = PathBuf::new();
    while i < comps.len() {
        match comps[i] {
            Component::Prefix(_) | Component::RootDir => {
                acc.push(comps[i].as_os_str());
                i += 1;
            }
            _ => break,
        }
    }
    while i < comps.len() {
        match comps[i] {
            Component::CurDir => i += 1,
            Component::ParentDir => {
                acc.pop();
                i += 1;
            }
            Component::Normal(name) => {
                let next = acc.join(name);
                if next.exists() {
                    acc = canonicalize_component(&next);
                    i += 1;
                } else {
                    return append_lexical(acc, &comps[i..]);
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                acc.push(comps[i].as_os_str());
                i += 1;
            }
        }
    }
    acc
}

/// Resolve a path for fence prefix checks (lexical `..`, partial canonicalize).
pub fn resolve_fence_path(raw: &str, base: &Path) -> PathBuf {
    resolve_components(&join_base(raw, base))
}

fn resolved_clean(path: &Path) -> PathBuf {
    resolve_components(path)
}

fn path_allowed(resolved: &Path, cwd: &Path, allowed_extra: &[PathBuf]) -> bool {
    let cwd_root = resolved_clean(cwd);
    if path_starts_with(resolved, &cwd_root) {
        return true;
    }
    allowed_extra.iter().any(|extra| {
        let root = if extra.is_absolute() {
            resolved_clean(extra)
        } else {
            resolved_clean(&cwd.join(extra))
        };
        path_starts_with(resolved, &root)
    })
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
            let Ok(meta) = target.symlink_metadata() else {
                continue;
            };
            if meta.file_type().is_symlink() {
                record_symlink(&mut digest, &target);
                continue;
            }
            if meta.is_file() {
                record_file(&mut digest, &target);
            } else if meta.is_dir() {
                walk_dir(&mut digest, &target);
            }
        }
    }
    digest
}

const SKIP_WALK_DIR_NAMES: &[&str] = &[".git", "target", "node_modules"];

fn symlink_target_hash(path: &Path) -> u64 {
    let Ok(link) = std::fs::read_link(path) else {
        return 0;
    };
    let mut hasher = DefaultHasher::new();
    link.to_string_lossy().hash(&mut hasher);
    hasher.finish()
}

fn record_symlink(digest: &mut Digest, path: &Path) {
    digest.insert(
        path.to_path_buf(),
        FileMeta {
            size: 0,
            content_hash: symlink_target_hash(path),
        },
    );
}

fn record_file(digest: &mut Digest, path: &Path) {
    let Ok(meta) = path.symlink_metadata() else {
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return;
    }
    digest.insert(
        path.to_path_buf(),
        FileMeta {
            size: meta.len(),
            content_hash: content_hash(path),
        },
    );
}

fn walk_dir(digest: &mut Digest, dir: &Path) {
    let entries = std::fs::read_dir(dir);
    let Ok(entries) = entries else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            record_symlink(digest, &path);
            continue;
        }
        if meta.is_file() {
            record_file(digest, &path);
        } else if meta.is_dir() {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| SKIP_WALK_DIR_NAMES.contains(&name))
            {
                continue;
            }
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
        .arg("--porcelain=v1")
        .arg("-z")
        .arg("--untracked-files=all")
        .output()
        .await;
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for rel in parse_git_porcelain_v1_z(&output.stdout) {
        if !in_fence(&rel, cwd, fence) {
            paths.push(rel);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Paths from `git status --porcelain=v1 -z` (destination path for renames).
pub fn parse_git_porcelain_v1_z(output: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut i = 0;
    while i < output.len() {
        if i + 2 > output.len() {
            break;
        }
        let status0 = output[i];
        let status1 = output[i + 1];
        i += 2;
        if i < output.len() && output[i] == b' ' {
            i += 1;
        }
        let is_rename = matches!(status0, b'R' | b'C') || matches!(status1, b'R' | b'C');
        let mut fields = Vec::new();
        while i < output.len() {
            if output[i] == 0 {
                i += 1;
                if fields.is_empty() {
                    continue;
                }
                break;
            }
            let start = i;
            while i < output.len() && output[i] != 0 {
                i += 1;
            }
            fields.push(String::from_utf8_lossy(&output[start..i]).into_owned());
            if i < output.len() {
                i += 1;
            }
            if is_rename {
                if fields.len() >= 2 {
                    break;
                }
            } else if !fields.is_empty() {
                break;
            }
        }
        if is_rename && fields.len() >= 2 {
            for path in fields {
                if !path.is_empty() {
                    paths.push(path);
                }
            }
        } else if let Some(path) = fields.into_iter().next() {
            if !path.is_empty() {
                paths.push(path);
            }
        }
    }
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
    use std::process::Command;
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
    fn dotdot_escape_to_new_file_outside_cwd() {
        let cwd = unique_temp("fence-dotdot");
        let sibling = unique_temp("fence-dotdot-out");
        fs::create_dir_all(&sibling).unwrap();
        let rel = format!(
            "../{}/new.txt",
            sibling.file_name().unwrap().to_string_lossy()
        );
        let args = serde_json::json!({"path": rel})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("edit", &args, &cwd, &[]).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_dotdot_escape_for_new_file() {
        use std::os::unix::fs::symlink;
        let cwd = unique_temp("fence-slink-dotdot");
        let outside = unique_temp("fence-slink-dotdot-out");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, cwd.join("link")).unwrap();
        let args = serde_json::json!({"path": "link/../evil.txt"})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("edit", &args, &cwd, &[]).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_escape_for_new_file() {
        use std::os::unix::fs::symlink;
        let cwd = unique_temp("fence-slink-cwd");
        let outside = unique_temp("fence-slink-out");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, cwd.join("link")).unwrap();
        let args = serde_json::json!({"path": "link/new.txt"})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("edit", &args, &cwd, &[]).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_session_dir_allows_new_file() {
        use std::os::unix::fs::symlink;
        let cwd = unique_temp("fence-sess-cwd");
        let outside = unique_temp("fence-sess-out");
        fs::create_dir_all(&outside).unwrap();
        let session = cwd.join("session-link");
        symlink(&outside, &session).unwrap();
        let target = outside.join("note.txt");
        let args = serde_json::json!({"path": target.display().to_string()})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("write", &args, &cwd, &[session]).is_none());
    }

    #[test]
    fn shell_checks_all_cwd_keys() {
        let cwd = unique_temp("fence-shell");
        let outside = unique_temp("fence-shell-out");
        fs::create_dir_all(&outside).unwrap();
        let args = serde_json::json!({
            "cwd": cwd.display().to_string(),
            "working_directory": outside.display().to_string(),
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(tool_escape("shell", &args, &cwd, &[]).is_some());
    }

    #[test]
    fn parse_porcelain_z_space_and_rename() {
        let mut raw = b"?? out side.txt\0".to_vec();
        raw.extend_from_slice(b"R  in.txt\0outside.txt\0");
        let paths = parse_git_porcelain_v1_z(&raw);
        assert!(paths.contains(&"out side.txt".to_string()));
        assert!(paths.contains(&"in.txt".to_string()));
        assert!(paths.contains(&"outside.txt".to_string()));
        let cwd = unique_temp("fence-parse-rename");
        let fence = vec!["in.txt".into()];
        let drift: Vec<_> = paths
            .iter()
            .filter(|path| !in_fence(path, &cwd, &fence))
            .cloned()
            .collect();
        assert_eq!(
            drift,
            vec!["out side.txt".to_string(), "outside.txt".to_string()]
        );
        let paths2 = parse_git_porcelain_v1_z(b"R  outside2.txt\0in.txt\0");
        let drift2: Vec<_> = paths2
            .iter()
            .filter(|path| !in_fence(path, &cwd, &fence))
            .cloned()
            .collect();
        assert_eq!(drift2, vec!["outside2.txt".to_string()]);
    }

    #[test]
    fn drift_path_with_space() {
        let cwd = unique_temp("fence-space");
        init_git(&cwd);
        fs::write(cwd.join("in.txt"), b"x").unwrap();
        Command::new("git")
            .args(["add", "in.txt"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&cwd)
            .output()
            .unwrap();
        fs::write(cwd.join("out side.txt"), b"y").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let paths = rt.block_on(drift(&cwd, &["in.txt".into()]));
        assert_eq!(paths, vec!["out side.txt".to_string()]);
    }

    #[test]
    fn path_arg_keys_all_checked_for_escape() {
        let cwd = unique_temp("fence-path-keys");
        let outside = unique_temp("fence-path-keys-out");
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("x");
        let args = serde_json::json!({"filePath": outside_file.display().to_string()})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("edit", &args, &cwd, &[]).is_some());
        let args = serde_json::json!({"targetDirectory": outside.display().to_string()})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("write", &args, &cwd, &[]).is_some());
        let args = serde_json::json!({"arguments": {"path": outside_file.display().to_string()}})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("edit", &args, &cwd, &[]).is_some());
        fs::write(cwd.join("inside.txt"), b"i").unwrap();
        let args = serde_json::json!({
            "path": "inside.txt",
            "file_path": outside_file.display().to_string(),
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(tool_escape("edit", &args, &cwd, &[]).is_some());
    }

    #[test]
    fn snapshot_skips_vcs_and_vendor_dirs() {
        let root = unique_temp("fence-snap-skip");
        let fenced = root.join("pkg");
        fs::create_dir_all(fenced.join(".git")).unwrap();
        fs::create_dir_all(fenced.join("target")).unwrap();
        fs::create_dir_all(fenced.join("node_modules")).unwrap();
        fs::write(fenced.join(".git/config"), b"1").unwrap();
        fs::write(fenced.join("target/lib.rlib"), b"2").unwrap();
        fs::write(fenced.join("node_modules/x.js"), b"3").unwrap();
        fs::write(fenced.join("ok.txt"), b"ok").unwrap();
        let before = snapshot(&[root.clone()], &["pkg".into()], &root);
        fs::write(fenced.join(".git/config"), b"changed").unwrap();
        fs::write(fenced.join("target/lib.rlib"), b"changed").unwrap();
        fs::write(fenced.join("node_modules/x.js"), b"changed").unwrap();
        let after = snapshot(&[root.clone()], &["pkg".into()], &root);
        assert!(changed(&before, &after).is_empty());
        fs::write(fenced.join("ok.txt"), b"changed").unwrap();
        let after2 = snapshot(&[root.clone()], &["pkg".into()], &root);
        assert_eq!(changed(&after, &after2), vec![fenced.join("ok.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_records_symlink_without_following() {
        use std::os::unix::fs::symlink;
        let root = unique_temp("fence-snap-link");
        let fenced = root.join("tree");
        fs::create_dir_all(&fenced).unwrap();
        let outside = unique_temp("fence-snap-link-out");
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        symlink(&outside, fenced.join("linkdir")).unwrap();
        let before = snapshot(&[root.clone()], &["tree".into()], &root);
        let other = unique_temp("fence-snap-link-other");
        fs::create_dir_all(&other).unwrap();
        fs::remove_file(fenced.join("linkdir")).unwrap();
        symlink(&other, fenced.join("linkdir")).unwrap();
        let after = snapshot(&[root.clone()], &["tree".into()], &root);
        assert_eq!(changed(&before, &after), vec![fenced.join("linkdir")]);
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
