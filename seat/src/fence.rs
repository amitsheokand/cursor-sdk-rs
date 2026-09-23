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
    protected_roots: &[PathBuf],
) -> Option<PathBuf> {
    let name = tool_name.trim().to_lowercase();
    if is_read_only(&name) {
        return None;
    }
    if name == "shell" || name.ends_with("shell") {
        return shell_escape(args, cwd, allowed_extra, protected_roots);
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

fn collect_shell_cwd_values(args: &serde_json::Map<String, Value>, out: &mut Vec<String>) {
    const CWD_KEYS: &[&str] = &["cwd", "working_directory", "workingDirectory"];
    for key in CWD_KEYS {
        if let Some(value) = args.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    if let Some(Value::Object(nested)) = args.get("arguments") {
        for key in CWD_KEYS {
            if let Some(value) = nested.get(*key).and_then(Value::as_str) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
    }
}

fn shell_effective_cwd(args: &serde_json::Map<String, Value>, worktree_cwd: &Path) -> PathBuf {
    let mut values = Vec::new();
    collect_shell_cwd_values(args, &mut values);
    if let Some(raw) = values.into_iter().next() {
        return resolve_fence_path(&raw, worktree_cwd);
    }
    resolved_clean(worktree_cwd)
}

fn shell_escape(
    args: &serde_json::Map<String, Value>,
    cwd: &Path,
    allowed_extra: &[PathBuf],
    protected_roots: &[PathBuf],
) -> Option<PathBuf> {
    let mut cwd_values = Vec::new();
    collect_shell_cwd_values(args, &mut cwd_values);
    for raw in &cwd_values {
        if let Some(hit) = resolve_escape(raw, cwd, allowed_extra) {
            return Some(hit);
        }
    }
    if protected_roots.is_empty() {
        return None;
    }
    let effective = shell_effective_cwd(args, cwd);
    for cmd in shell_command_strings(args) {
        if let Some(hit) =
            shell_command_protected_escape(&cmd, &effective, cwd, allowed_extra, protected_roots)
        {
            return Some(hit);
        }
    }
    None
}

const SHELL_CMD_KEYS: &[&str] = &["command", "cmd", "script"];

fn shell_command_strings(args: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    for key in SHELL_CMD_KEYS {
        if let Some(value) = args.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    if let Some(Value::Object(nested)) = args.get("arguments") {
        for key in SHELL_CMD_KEYS {
            if let Some(value) = nested.get(*key).and_then(Value::as_str) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
    }
    out
}

fn is_unquoted_shell_break(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')' | '\n' | '`')
}

/// Split a shell command on whitespace and shell metacharacters; honour quotes and `\`.
pub fn tokenize_shell_command(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    tokenize_shell_command_into(cmd, &mut tokens);
    tokens
}

fn tokenize_shell_command_into(cmd: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if is_unquoted_shell_break(chars[i]) {
            i += 1;
            continue;
        }
        if chars[i] == '\'' {
            let (content, next) = read_single_quoted(&chars, i + 1);
            push_quoted_shell_token(&content, out);
            i = next;
            continue;
        }
        if chars[i] == '"' {
            let (content, next) = read_double_quoted(&chars, i + 1);
            push_quoted_shell_token(&content, out);
            i = next;
            continue;
        }
        let (word, next) = read_unquoted_word(&chars, i);
        if !word.is_empty() {
            out.push(word);
        }
        i = next;
    }
}

fn push_quoted_shell_token(content: &str, out: &mut Vec<String>) {
    if content.is_empty() {
        return;
    }
    out.push(content.to_string());
    tokenize_shell_command_into(content, out);
}

fn read_single_quoted(chars: &[char], start: usize) -> (String, usize) {
    let mut content = String::new();
    let mut i = start;
    while i < chars.len() && chars[i] != '\'' {
        content.push(chars[i]);
        i += 1;
    }
    let end = if i < chars.len() { i + 1 } else { i };
    (content, end)
}

fn read_double_quoted(chars: &[char], start: usize) -> (String, usize) {
    let mut content = String::new();
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            content.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if chars[i] == '"' {
            return (content, i + 1);
        }
        content.push(chars[i]);
        i += 1;
    }
    (content, i)
}

fn read_unquoted_word(chars: &[char], start: usize) -> (String, usize) {
    let mut word = String::new();
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            word.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if is_unquoted_shell_break(chars[i]) || chars[i] == '\'' || chars[i] == '"' {
            break;
        }
        word.push(chars[i]);
        i += 1;
    }
    (word, i)
}

fn path_candidates_from_token(token: &str) -> Vec<String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut out = vec![trimmed.to_string()];
    if let Some((_, value)) = trimmed.split_once('=') {
        let v = value.trim();
        if !v.is_empty() {
            out.push(v.to_string());
            for field in v.split(':') {
                let f = field.trim();
                if !f.is_empty() {
                    out.push(f.to_string());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_absolute_or_home_spelling(token: &str) -> bool {
    let t = token.trim();
    t.starts_with('/')
        || t.starts_with('~')
        || t.starts_with("$HOME")
        || t.starts_with("${HOME}")
}

fn is_relative_path_spelling(token: &str) -> bool {
    let t = token.trim();
    !t.is_empty()
        && !is_absolute_or_home_spelling(t)
        && (t.starts_with('.') || t.contains('/') || t.contains('\\'))
}

/// Lexical path for an absolute or home-relative shell token (no symlink follow).
fn lexical_shell_path(token: &str) -> Option<PathBuf> {
    let t = token.trim().trim_matches(|c| c == '\'' || c == '"');
    if t.starts_with('/') {
        return Some(lexically_normalize(Path::new(t)));
    }
    if t == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return Some(lexically_normalize(Path::new(&home)));
        }
        return None;
    }
    if let Some(rest) = t.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Some(lexically_normalize(&PathBuf::from(home).join(rest)));
        }
        return None;
    }
    if t == "$HOME" || t == "${HOME}" {
        if let Ok(home) = std::env::var("HOME") {
            return Some(lexically_normalize(Path::new(&home)));
        }
        return None;
    }
    if let Some(rest) = t.strip_prefix("$HOME/") {
        if let Ok(home) = std::env::var("HOME") {
            return Some(lexically_normalize(&PathBuf::from(home).join(rest)));
        }
        return None;
    }
    if let Some(rest) = t.strip_prefix("${HOME}/") {
        if let Ok(home) = std::env::var("HOME") {
            return Some(lexically_normalize(&PathBuf::from(home).join(rest)));
        }
    }
    None
}

fn path_under_root(path: &Path, root: &Path) -> bool {
    path == root || path_starts_with(path, root)
}

fn hits_protected_root(
    path: &Path,
    protected_roots: &[PathBuf],
    cwd: &Path,
    allowed_extra: &[PathBuf],
) -> Option<PathBuf> {
    if path_allowed(path, cwd, allowed_extra) {
        return None;
    }
    for root in protected_roots {
        let lexical_root = lexically_normalize(root);
        let canon_root = resolved_clean(root);
        if path_under_root(path, &lexical_root) || path_under_root(path, &canon_root) {
            return Some(path.to_path_buf());
        }
    }
    None
}

fn lexical_relative_shell_path(token: &str, base: &Path) -> PathBuf {
    lexically_normalize(&join_base(token.trim(), base))
}

fn resolve_shell_candidate_path(candidate: &str, lexical_base: &Path) -> Option<PathBuf> {
    if is_absolute_or_home_spelling(candidate) {
        lexical_shell_path(candidate)
    } else if is_relative_path_spelling(candidate) {
        Some(lexical_relative_shell_path(candidate, lexical_base))
    } else {
        None
    }
}

fn is_cd_builtin(token: &str) -> bool {
    token.eq_ignore_ascii_case("cd") || token.eq_ignore_ascii_case("pushd")
}

fn shell_skip_token(token: &str) -> bool {
    let t = token.trim();
    t.is_empty() || t == "&" || t == "&&" || t == ";" || t == "|"
}

fn next_shell_arg(tokens: &[String], from: usize) -> Option<(String, usize)> {
    let mut i = from;
    while i < tokens.len() {
        if shell_skip_token(&tokens[i]) {
            i += 1;
            continue;
        }
        return Some((tokens[i].clone(), i + 1));
    }
    None
}

fn apply_cd_to_lexical_base(path_token: &str, lexical_base: &Path) -> PathBuf {
    if let Some(path) = resolve_shell_candidate_path(path_token, lexical_base) {
        path
    } else {
        lexical_relative_shell_path(path_token, lexical_base)
    }
}

fn shell_command_protected_escape(
    cmd: &str,
    effective_cwd: &Path,
    worktree_cwd: &Path,
    allowed_extra: &[PathBuf],
    protected_roots: &[PathBuf],
) -> Option<PathBuf> {
    let tokens = tokenize_shell_command(cmd);
    let mut lexical_base = effective_cwd.to_path_buf();
    let mut i = 0;
    while i < tokens.len() {
        if is_cd_builtin(&tokens[i]) {
            i += 1;
            if let Some((path_token, next)) = next_shell_arg(&tokens, i) {
                for candidate in path_candidates_from_token(&path_token) {
                    if let Some(path) = resolve_shell_candidate_path(&candidate, &lexical_base) {
                        if let Some(hit) = hits_protected_root(
                            &path,
                            protected_roots,
                            worktree_cwd,
                            allowed_extra,
                        ) {
                            return Some(hit);
                        }
                    }
                }
                lexical_base = apply_cd_to_lexical_base(&path_token, &lexical_base);
                i = next;
            }
            continue;
        }
        for candidate in path_candidates_from_token(&tokens[i]) {
            if let Some(path) = resolve_shell_candidate_path(&candidate, &lexical_base) {
                if let Some(hit) =
                    hits_protected_root(&path, protected_roots, worktree_cwd, allowed_extra)
                {
                    return Some(hit);
                }
            }
        }
        i += 1;
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

fn containing_protected_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    for root in roots {
        let lexical_root = lexically_normalize(root);
        let canon_root = resolved_clean(root);
        if path_under_root(path, &lexical_root) || path_under_root(path, &canon_root) {
            return Some(root);
        }
    }
    None
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|meta| meta.file_type().is_file() && !meta.file_type().is_symlink())
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn rel_path_git_spec(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `Ok(None)` when `rel` is absent at HEAD; `Err` when git fails.
fn git_head_bytes(cwd: &Path, rel: &Path) -> Result<Option<Vec<u8>>, ()> {
    use std::process::Command;
    let spec = format!("HEAD:{}", rel_path_git_spec(rel));
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("show")
        .arg(&spec)
        .output()
        .map_err(|_| ())?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("does not exist") || stderr.contains("exists on disk") {
        return Ok(None);
    }
    Err(())
}

/// Worktree file differs from `git HEAD:<rel>` (or is new at HEAD). Git failure → not authored.
fn worktree_file_agent_authored(cwd: &Path, rel: &Path, worktree_copy: &Path) -> bool {
    let worktree_hash = content_hash(worktree_copy);
    match git_head_bytes(cwd, rel) {
        Err(()) => false,
        Ok(None) => true,
        Ok(Some(head_bytes)) => worktree_hash != hash_bytes(&head_bytes),
    }
}

/// Classify protected-root snapshot deltas: agent copy (worktree hash match) vs external.
pub fn attribute(
    changed: &[PathBuf],
    after: &Digest,
    roots: &[PathBuf],
    cwd: &Path,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut escapes = Vec::new();
    let mut external = Vec::new();
    for path in changed {
        let Some(root) = containing_protected_root(path, roots) else {
            external.push(path.clone());
            continue;
        };
        let Ok(rel) = path.strip_prefix(root) else {
            external.push(path.clone());
            continue;
        };
        let worktree_copy = cwd.join(rel);
        if !is_regular_file(path) || !is_regular_file(&worktree_copy) {
            external.push(path.clone());
            continue;
        }
        let Some(protected_meta) = after.get(path) else {
            external.push(path.clone());
            continue;
        };
        if protected_meta.content_hash == content_hash(&worktree_copy) {
            if worktree_file_agent_authored(cwd, rel, &worktree_copy) {
                escapes.push(path.clone());
            } else {
                external.push(path.clone());
            }
        } else {
            external.push(path.clone());
        }
    }
    (escapes, external)
}

/// Advance the mid-run baseline for externally changed paths so they are not re-reported.
pub fn rebaseline_entries(baseline: &mut Digest, after: &Digest, paths: &[PathBuf]) {
    for path in paths {
        match after.get(path) {
            Some(meta) => {
                baseline.insert(path.clone(), meta.clone());
            }
            None => {
                baseline.remove(path);
            }
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

    fn git_commit_all(dir: &Path, message: &str) {
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(dir)
            .output()
            .expect("git commit");
    }

    #[test]
    fn tool_escape_blocks_edit_outside_cwd() {
        let cwd = unique_temp("fence-cwd");
        let args = serde_json::json!({"path": "/etc/passwd"})
            .as_object()
            .unwrap()
            .clone();
        let hit = tool_escape("edit", &args, &cwd, &[], &[]);
        assert!(hit.is_some());
    }

    #[test]
    fn tool_escape_allows_read_outside() {
        let cwd = unique_temp("fence-cwd-read");
        let args = serde_json::json!({"path": "/etc/passwd"})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("read", &args, &cwd, &[], &[]).is_none());
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
    fn attribute_worktree_copy_is_escape() {
        let wt = unique_temp("attr-wt");
        let root = unique_temp("attr-root");
        init_git(&wt);
        fs::write(wt.join("f.txt"), b"head").unwrap();
        git_commit_all(&wt, "base");
        fs::write(wt.join("f.txt"), b"same").unwrap();
        fs::write(root.join("f.txt"), b"old").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::write(root.join("f.txt"), b"same").unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert_eq!(external, Vec::<PathBuf>::new());
        assert_eq!(escapes, vec![root.join("f.txt")]);
    }

    #[test]
    fn attribute_primary_revert_to_head_while_worktree_clean_is_external() {
        let wt = unique_temp("attr-wt-revert");
        let root = unique_temp("attr-root-revert");
        init_git(&wt);
        fs::write(wt.join("f.txt"), b"head").unwrap();
        git_commit_all(&wt, "base");
        fs::write(root.join("f.txt"), b"dirty").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::write(root.join("f.txt"), b"head").unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert!(escapes.is_empty());
        assert_eq!(external, vec![root.join("f.txt")]);
    }

    #[test]
    fn attribute_new_untracked_worktree_copy_is_escape() {
        let wt = unique_temp("attr-wt-new");
        let root = unique_temp("attr-root-new");
        init_git(&wt);
        fs::write(wt.join("README.md"), b"init").unwrap();
        git_commit_all(&wt, "empty");
        fs::write(wt.join("f.txt"), b"new-agent").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::write(root.join("f.txt"), b"new-agent").unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert_eq!(external, Vec::<PathBuf>::new());
        assert_eq!(escapes, vec![root.join("f.txt")]);
    }

    #[test]
    fn attribute_different_content_is_external() {
        let wt = unique_temp("attr-wt-diff");
        let root = unique_temp("attr-root-diff");
        fs::write(wt.join("f.txt"), b"wt").unwrap();
        fs::write(root.join("f.txt"), b"old").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::write(root.join("f.txt"), b"owner").unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert!(escapes.is_empty());
        assert_eq!(external, vec![root.join("f.txt")]);
    }

    #[test]
    fn attribute_missing_worktree_is_external() {
        let wt = unique_temp("attr-wt-miss");
        let root = unique_temp("attr-root-miss");
        fs::write(root.join("f.txt"), b"old").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::write(root.join("f.txt"), b"new").unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert!(escapes.is_empty());
        assert_eq!(external.len(), 1);
    }

    #[test]
    fn attribute_deletion_in_root_is_external() {
        let wt = unique_temp("attr-wt-del");
        let root = unique_temp("attr-root-del");
        fs::write(wt.join("f.txt"), b"x").unwrap();
        fs::write(root.join("f.txt"), b"x").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::remove_file(root.join("f.txt")).unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert!(escapes.is_empty());
        assert_eq!(external, vec![root.join("f.txt")]);
    }

    #[cfg(unix)]
    #[test]
    fn attribute_symlink_in_root_is_external() {
        use std::os::unix::fs::symlink;
        let wt = unique_temp("attr-wt-link");
        let root = unique_temp("attr-root-link");
        fs::write(wt.join("f.txt"), b"x").unwrap();
        fs::write(root.join("f.txt"), b"x").unwrap();
        let roots = vec![root.clone()];
        let before = snapshot(&roots, &["f.txt".into()], &wt);
        fs::remove_file(root.join("f.txt")).unwrap();
        symlink(wt.join("f.txt"), root.join("f.txt")).unwrap();
        let after = snapshot(&roots, &["f.txt".into()], &wt);
        let delta = changed(&before, &after);
        let (escapes, external) = attribute(&delta, &after, &roots, &wt);
        assert!(escapes.is_empty());
        assert_eq!(external, vec![root.join("f.txt")]);
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
        assert!(tool_escape("edit", &args, &cwd, &[], &[]).is_some());
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
        assert!(tool_escape("edit", &args, &cwd, &[], &[]).is_some());
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
        assert!(tool_escape("edit", &args, &cwd, &[], &[]).is_some());
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
        assert!(tool_escape("write", &args, &cwd, &[session], &[]).is_none());
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
        assert!(tool_escape("shell", &args, &cwd, &[], &[]).is_some());
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
        assert!(tool_escape("edit", &args, &cwd, &[], &[]).is_some());
        let args = serde_json::json!({"targetDirectory": outside.display().to_string()})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("write", &args, &cwd, &[], &[]).is_some());
        let args = serde_json::json!({"arguments": {"path": outside_file.display().to_string()}})
            .as_object()
            .unwrap()
            .clone();
        assert!(tool_escape("edit", &args, &cwd, &[], &[]).is_some());
        fs::write(cwd.join("inside.txt"), b"i").unwrap();
        let args = serde_json::json!({
            "path": "inside.txt",
            "file_path": outside_file.display().to_string(),
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(tool_escape("edit", &args, &cwd, &[], &[]).is_some());
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

    fn shell_args(command: &str) -> serde_json::Map<String, Value> {
        serde_json::json!({"command": command})
            .as_object()
            .unwrap()
            .clone()
    }

    fn shell_hit(command: &str, cwd: &Path, protected: &[PathBuf]) -> Option<PathBuf> {
        tool_escape("shell", &shell_args(command), cwd, &[], protected)
    }

    #[test]
    fn shell_command_cp_into_protected_root() {
        let cwd = unique_temp("shell-cp-cwd");
        let main = unique_temp("shell-cp-main");
        fs::create_dir_all(main.join("crates/foo/src")).unwrap();
        let src = cwd.join("a.rs");
        fs::write(&src, b"x").unwrap();
        let cmd = format!(
            "cp {} {}",
            src.display(),
            main.join("crates/foo/src/dde_lparam.rs").display()
        );
        assert!(shell_hit(&cmd, &cwd, &[main.clone()]).is_some());
    }

    #[test]
    fn shell_command_cd_into_protected_root() {
        let cwd = unique_temp("shell-cd-cwd");
        let main = unique_temp("shell-cd-main");
        fs::create_dir_all(&main).unwrap();
        let cmd = format!("cd {} && cargo test", main.display());
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_command_git_checkout_in_protected_root() {
        let cwd = unique_temp("shell-git-cwd");
        let main = unique_temp("shell-git-main");
        fs::create_dir_all(main.join("crates/ffi")).unwrap();
        let cmd = format!(
            "cd {} && git checkout -- crates/ffi/ffi.rs",
            main.display()
        );
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_command_cargo_target_dir_env() {
        let cwd = unique_temp("shell-cargo-cwd");
        let main = unique_temp("shell-cargo-main");
        fs::create_dir_all(main.join("target")).unwrap();
        let cmd = format!("CARGO_TARGET_DIR={}/target cargo test", main.display());
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_command_manifest_path_flag() {
        let cwd = unique_temp("shell-manifest-cwd");
        let main = unique_temp("shell-manifest-main");
        fs::write(main.join("Cargo.toml"), b"[package]\nname=\"x\"\n").unwrap();
        let cmd = format!("cargo test --manifest-path={}/Cargo.toml", main.display());
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_command_home_spelling_variants() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let main = home.join(format!("cursor-seat-shell-home-main-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&main);
        fs::create_dir_all(&main).unwrap();
        let cwd = unique_temp("shell-home-cwd");
        let rel = main.strip_prefix(&home).expect("under home");
        for cmd in [
            format!("cat ~/{}/secret", rel.to_string_lossy()),
            format!("cat $HOME/{}/secret", rel.to_string_lossy()),
            format!("cat ${{HOME}}/{}/secret", rel.to_string_lossy()),
        ] {
            assert!(shell_hit(&cmd, &cwd, &[main.clone()]).is_some());
        }
    }

    #[test]
    fn shell_command_quoted_absolute_path() {
        let cwd = unique_temp("shell-quote-cwd");
        let main = unique_temp("shell-quote-main");
        fs::create_dir_all(&main).unwrap();
        let cmd = format!("cat '{}'", main.join("x.txt").display());
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_command_bash_lc_nested() {
        let cwd = unique_temp("shell-bash-cwd");
        let main = unique_temp("shell-bash-main");
        fs::create_dir_all(&main).unwrap();
        let inner = format!("cd {} && touch t", main.display());
        let cmd = format!("bash -lc '{}'", inner);
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_command_allows_tmp_and_nix() {
        let cwd = unique_temp("shell-ok-cwd");
        let main = unique_temp("shell-ok-main");
        assert!(shell_hit("cat /tmp/foo", &cwd, &[main.clone()]).is_none());
        assert!(shell_hit("ls /nix/store/abc", &cwd, &[main]).is_none());
    }

    #[test]
    fn shell_command_prefix_not_string_substring() {
        let cwd = unique_temp("shell-prefix-cwd");
        let root = unique_temp("shell-prefix-root");
        let sibling = unique_temp("shell-prefix-sibling");
        let name = root.file_name().unwrap().to_string_lossy();
        let docs = sibling.parent().unwrap().join(format!("{name}-docs"));
        let _ = fs::remove_dir_all(&docs);
        fs::create_dir_all(&docs).unwrap();
        let cmd = format!("cat {}/x", docs.display());
        assert!(shell_hit(&cmd, &cwd, &[root]).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn shell_command_ignores_relative_symlink_into_root() {
        use std::os::unix::fs::symlink;
        let cwd = unique_temp("shell-rel-cwd");
        let main = unique_temp("shell-rel-main");
        fs::create_dir_all(&main).unwrap();
        symlink(&main, cwd.join("sdk")).unwrap();
        assert!(shell_hit("cat sdk/secret.txt", &cwd, &[main]).is_none());
    }

    #[test]
    fn shell_command_path_under_cwd_sibling_of_root() {
        let parent = unique_temp("shell-sib-parent");
        let main = parent.join("main");
        let wt = parent.join("wt");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join("ok.txt"), b"ok").unwrap();
        let cmd = format!("cat {}/ok.txt", wt.display());
        assert!(shell_hit(&cmd, &wt, &[main]).is_none());
    }

    #[test]
    fn shell_command_relative_dotdot_into_protected_root() {
        let parent = unique_temp("shell-rel-parent");
        let main = parent.join("main");
        let wt = parent.join("wt");
        fs::create_dir_all(main.join("target")).unwrap();
        fs::create_dir_all(&wt).unwrap();
        assert!(shell_hit("CARGO_TARGET_DIR=../main/target cargo test", &wt, &[main.clone()]).is_some());
        assert!(shell_hit("cd ../main && git status", &wt, &[main.clone()]).is_some());
        assert!(shell_hit("cat ../main/x", &wt, &[main]).is_some());
    }

    #[test]
    fn shell_command_ln_relative_into_root() {
        let parent = unique_temp("shell-ln-parent");
        let main = parent.join("main");
        let wt = parent.join("wt");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&wt).unwrap();
        assert!(shell_hit("ln -sfn ../main/sdk sdk", &wt, &[main.clone()]).is_some());
        assert!(shell_hit("cat sdk/secret.txt", &wt, &[main]).is_none());
    }

    #[test]
    fn shell_command_path_colon_field_hits_root() {
        let cwd = unique_temp("shell-path-colon-cwd");
        let main = unique_temp("shell-path-colon-main");
        fs::create_dir_all(main.join("bin")).unwrap();
        let cmd = format!("PATH=$PATH:{}", main.join("bin").display());
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }

    #[test]
    fn shell_nested_arguments_working_directory_escape() {
        let cwd = unique_temp("shell-nested-cwd");
        let main = unique_temp("shell-nested-main");
        fs::create_dir_all(&main).unwrap();
        let args = serde_json::json!({
            "command": "true",
            "arguments": {"working_directory": main.display().to_string()},
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(tool_escape("shell", &args, &cwd, &[], &[]).is_some());
    }

    #[test]
    fn shell_cd_updates_lexical_base_for_later_tokens() {
        let root = unique_temp("cd-track-root");
        let primary = root.join("primary");
        let wt = root.join("wt");
        fs::create_dir_all(wt.join("seat")).unwrap();
        fs::create_dir_all(&primary).unwrap();
        fs::write(primary.join("file"), b"x").unwrap();
        assert!(shell_hit("cd seat && cp ../../primary/file .", &wt, &[primary.clone()]).is_some());
        fs::write(wt.join("README.md"), b"r").unwrap();
        let outside = unique_temp("cd-track-other");
        assert!(shell_hit("cd seat && cat ../README.md", &wt, &[outside]).is_none());
    }

    #[test]
    fn shell_quoted_path_with_space_in_root_name() {
        let cwd = unique_temp("shell-space-cwd");
        let main = unique_temp("shell space root");
        fs::create_dir_all(&main).unwrap();
        let cmd = format!("cat \"{}/x.txt\"", main.display());
        assert!(shell_hit(&cmd, &cwd, &[main]).is_some());
    }
}
