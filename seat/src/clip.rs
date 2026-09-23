//! Rune-safe head/tail clip, sha256 archive, and `sdkrun_v1` receipts.
//!
//! `bound_output` ports unreal-agent `harness/operation/output.go`
//! `BoundOutput`: the limit counts runes, head keeps `limit/2` runes,
//! tail keeps the rest, and the marker reports skipped **bytes**. The
//! only simplification is dropping Go's incremental head/tail split API
//! (built for streaming shell output); the seat clips a final string.
//! `wrap_receipt` is byte-compatible with `jobs/clip.py` so the Python
//! post gate reads seat archives unchanged.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Default output budget, matching `limits.clip_chars` and Go's
/// `DefaultMaxOutputLength`.
pub const DEFAULT_CLIP_CHARS: usize = 40_000;

/// Hard upper bound for any clip, matching Go's `MaxOutputLength`.
pub const MAX_OUTPUT_CHARS: usize = 1_000_000;

/// Rune slop reserved for the truncation marker when clipping to a hard
/// budget: the marker is at most ~32 runes (`...` + digits + text +
/// `...`), so 64 always covers it.
const MARKER_RESERVE: usize = 64;

/// Clip `text` to `limit` runes, head/tail split.
///
/// Returns the original text with `false` when it already fits.
/// Otherwise returns `head + "...{skipped} bytes truncated..." + tail`
/// with `true`, where head holds `limit/2` runes and tail holds
/// `limit - limit/2` runes.
pub fn bound_output(text: &str, limit: usize) -> (String, bool) {
    bound_output_inner(text, limit, None)
}

/// Clip like [`bound_output`], naming the archive holding the complete
/// output: the marker becomes
/// `...{skipped} bytes truncated; complete output in {path}...`.
pub fn bound_output_with_path(text: &str, limit: usize, path: &str) -> (String, bool) {
    bound_output_inner(text, limit, Some(path))
}

fn bound_output_inner(text: &str, limit: usize, path: Option<&str>) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    let head = take_runes(text, limit / 2, false);
    let tail = take_runes(text, limit - limit / 2, true);
    let skipped = text.len() - (head.len() + tail.len());
    let mut marker = format!("...{skipped} bytes truncated");
    if let Some(path) = path {
        marker.push_str("; complete output in ");
        marker.push_str(path);
    }
    let mut out = String::with_capacity(head.len() + marker.len() + 3 + tail.len());
    out.push_str(&head);
    out.push_str(&marker);
    out.push_str("...");
    out.push_str(&tail);
    (out, true)
}

/// First (`tail == false`) or last (`tail == true`) `count` runes,
/// always cut on a char boundary (`&str` is valid UTF-8 by construction).
fn take_runes(text: &str, count: usize, tail: bool) -> &str {
    if tail {
        let mut start = text.len();
        let mut remaining = count;
        while remaining > 0 {
            match text[..start].chars().next_back() {
                Some(last) => {
                    start -= last.len_utf8();
                    remaining -= 1;
                }
                None => break,
            }
        }
        &text[start..]
    } else {
        match text.char_indices().nth(count) {
            Some((index, _)) => &text[..index],
            None => text,
        }
    }
}

/// Clip `text` to `budget` runes **including** the marker.
///
/// Unlike [`bound_output`], whose marker overshoots the limit by ~32
/// runes, this reserves [`MARKER_RESERVE`] so the result fits `budget`
/// whenever the marker math holds (true for realistic budgets; degenerate
/// budgets under ~64 runes are best-effort).
pub fn clip_to_budget(text: &str, budget: usize) -> (String, bool) {
    if text.chars().count() <= budget {
        return (text.to_string(), false);
    }
    bound_output(text, budget.saturating_sub(MARKER_RESERVE))
}

/// Hex SHA-256 of `text`, matching `jobs/clip.py::sha256_text`.
pub fn sha256_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// An archived text plus its content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveResult {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// Write `text` to `<base_dir>/<subdir>/<session_id>/<file_name>`,
/// mirroring `jobs/clip.py::archive_body` (parents created, raw UTF-8
/// bytes, sha256 over the bytes).
pub fn archive_text(
    text: &str,
    base_dir: &Path,
    subdir: &str,
    session_id: &str,
    file_name: &str,
) -> std::io::Result<ArchiveResult> {
    let dir = base_dir.join(subdir).join(session_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file_name);
    let bytes = text.as_bytes();
    std::fs::write(&path, bytes)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(ArchiveResult {
        path,
        sha256: format!("{:x}", hasher.finalize()),
        bytes: bytes.len() as u64,
    })
}

/// Wrap `body` in an `sdkrun_v1` receipt header, byte-compatible with
/// `jobs/clip.py::wrap_receipt` (including the double space before
/// `OR`).
pub fn wrap_receipt(
    body: &str,
    archive: &ArchiveResult,
    via: &str,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
) -> String {
    let mut out = String::with_capacity(body.len() + 256);
    let _ = writeln!(out, "sdkrun_v1");
    let _ = writeln!(out, "source_sha256={}", archive.sha256);
    let _ = writeln!(out, "source_bytes={}", archive.bytes);
    let _ = writeln!(out, "source_artifact={}", archive.path.display());
    let _ = writeln!(out, "via={via}");
    if let Some(tokens) = tokens_before {
        let _ = writeln!(out, "tokens_before={tokens}");
    }
    if let Some(tokens) = tokens_after {
        let _ = writeln!(out, "tokens_after={tokens}");
    }
    let _ = writeln!(
        out,
        "recall=headroom_retrieve {}  OR  sed -n '1,80p' {}",
        archive.sha256,
        archive.path.display()
    );
    out.push_str("---\n");
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from unreal-agent harness/operation/output_test.go
    // TestBoundOutputPreservesHeadAndTail (invalid-UTF8 cases omitted:
    // Rust &str is always valid UTF-8).
    #[test]
    fn bound_output_matches_go_table() {
        let cases = [
            ("empty", "", "", 1, false),
            ("exact", "界é🙂", "界é🙂", 3, false),
            ("even", "abcdefghij", "abc...4 bytes truncated...hij", 6, true),
            ("odd", "abcdefghij", "ab...5 bytes truncated...hij", 5, true),
            ("one", "ab", "...1 bytes truncated...b", 1, true),
            ("two", "abc", "a...1 bytes truncated...c", 2, true),
            ("unicode", "界éab🙂好", "界...4 bytes truncated...🙂好", 3, true),
            ("ellipsis", "…abc…", "…...3 bytes truncated...…", 2, true),
            (
                "unicode and newline exact",
                "é\n🙂",
                "é\n🙂",
                3,
                false,
            ),
            (
                "unicode and newline truncated",
                "é\n🙂",
                "é...1 bytes truncated...🙂",
                2,
                true,
            ),
            (
                "unicode and whitespace",
                "é\n中🙂\t界",
                "é\n...7 bytes truncated...\t界",
                4,
                true,
            ),
            ("whitespace", "a\nbc\td", "a\n...2 bytes truncated...\td", 4, true),
            ("newline", "\n", "\n", 1, false),
            ("quote", "\"", "\"", 1, false),
            ("backslashes", "\\\\", "\\\\", 2, false),
            (
                "literal escape",
                "\\u1234",
                "\\...4 bytes truncated...4",
                2,
                true,
            ),
            (
                "control characters",
                "\x00ab\x1f",
                "\x00...2 bytes truncated...\x1f",
                2,
                true,
            ),
            (
                "control character tail",
                "\x00ab\x1f",
                "...3 bytes truncated...\x1f",
                1,
                true,
            ),
            ("unicode separators", "<>&\u{2028}\u{2029}", "<>&\u{2028}\u{2029}", 5, false),
            ("zero", "abc", "...3 bytes truncated...", 0, true),
        ];
        for (name, text, want, limit, truncated) in cases {
            let (got, was_truncated) = bound_output(text, limit);
            assert_eq!(got, want, "case {name}");
            assert_eq!(was_truncated, truncated, "case {name}");
        }
    }

    // Ported from TestBoundOutputCharacterBoundaries: head is a prefix,
    // tail a suffix, the marker count is the skipped bytes, each part
    // fills its rune budget.
    #[test]
    fn bound_output_keeps_valid_head_and_tail() {
        let texts = [
            "\"\\/\u{8}\u{c}\n\r\t\x00\x1f",
            "é界🙂…<>&\u{2028}\u{2029}",
            "abcdefghij",
        ];
        for text in texts {
            let runes = text.chars().count();
            for limit in 0..runes + 2 {
                let (got, truncated) = bound_output(text, limit);
                if !truncated {
                    assert_eq!(got, text, "limit {limit}");
                    assert!(runes <= limit, "limit {limit}");
                    continue;
                }
                let marker_start = got.find("...").expect("marker");
                let marker_end = got[marker_start + 3..]
                    .find("...")
                    .map(|i| marker_start + 3 + i + 3)
                    .expect("marker end");
                let (head, tail) = (&got[..marker_start], &got[marker_end..]);
                assert!(text.starts_with(head), "limit {limit}: {got:?}");
                assert!(text.ends_with(tail), "limit {limit}: {got:?}");
                let skipped: usize = got[marker_start + 3..marker_end - 3]
                    .split(' ')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(skipped, text.len() - head.len() - tail.len());
                assert_eq!(head.chars().count(), limit / 2, "limit {limit}");
                assert_eq!(tail.chars().count(), limit - limit / 2, "limit {limit}");
            }
        }
    }

    #[test]
    fn bound_output_with_path_names_the_archive() {
        let (got, truncated) = bound_output_with_path(&"界éab".repeat(10), 2, "/tmp/out");
        assert!(truncated);
        assert_eq!(
            got,
            "界...66 bytes truncated; complete output in /tmp/out...b"
        );
    }

    #[test]
    fn clip_to_budget_holds_the_hard_cap() {
        let text = "abcdefghij".repeat(1000);
        assert_eq!(text.chars().count(), 10_000);
        for budget in [64, 100, 1000, 9_999] {
            let (got, truncated) = clip_to_budget(&text, budget);
            assert!(truncated, "budget {budget}");
            assert!(
                got.chars().count() <= budget,
                "budget {budget}: {} runes",
                got.chars().count()
            );
        }
        let (got, truncated) = clip_to_budget("small", 100);
        assert!(!truncated);
        assert_eq!(got, "small");
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_text("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn receipt_is_byte_compatible_with_clip_py() {
        let archive = ArchiveResult {
            path: PathBuf::from("/tmp/base/sub/sess/out.txt"),
            sha256: "abc123".to_string(),
            bytes: 11,
        };
        // Expected output transcribed from jobs/clip.py::wrap_receipt
        // (note the double space before OR).
        let want = "sdkrun_v1\n\
            source_sha256=abc123\n\
            source_bytes=11\n\
            source_artifact=/tmp/base/sub/sess/out.txt\n\
            via=seat-rs\n\
            tokens_before=100\n\
            tokens_after=20\n\
            recall=headroom_retrieve abc123  OR  sed -n '1,80p' /tmp/base/sub/sess/out.txt\n\
            ---\n\
            hello world";
        assert_eq!(
            wrap_receipt("hello world", &archive, "seat-rs", Some(100), Some(20)),
            want
        );
        // Without token counts the lines are omitted, not blank.
        let minimal = wrap_receipt("b", &archive, "seat-rs", None, None);
        assert!(!minimal.contains("tokens_"));
    }

    #[test]
    fn archive_writes_bytes_and_hashes_them() {
        let dir = std::env::temp_dir().join(format!("seat-clip-{}", std::process::id()));
        let result =
            archive_text("héllo", &dir, "sub", "sess", "out.txt").expect("archive");
        assert_eq!(result.bytes, "héllo".len() as u64);
        assert_eq!(result.sha256, sha256_text("héllo"));
        assert_eq!(std::fs::read(&result.path).unwrap(), "héllo".as_bytes());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
