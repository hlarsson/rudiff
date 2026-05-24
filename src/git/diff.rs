//! Line-level diffing built on the `similar` crate.
//!
//! gix gives us *which* files changed and their blob ids; the actual line
//! pairing into hunks is done here. Two distinct diffs are produced:
//!
//! * the **display diff** (raw lines) used for rendering and stats, and
//! * the **canonical diff** (whitespace-normalized) used only to derive the
//!   stable [`content_hash`](super::model::FileChange::content_hash) that keys
//!   viewed-status. Normalizing means a whitespace-only edit hashes the same as
//!   no edit, so toggling whitespace-ignore never silently un-views a file.

use std::hash::Hasher;
use std::path::Path;

use similar::{ChangeTag, TextDiff};

use super::model::{DiffLine, FileDiff, Hunk};

/// Context radius used both for the canonical hash and as the default fold
/// radius in the UI.
pub const CONTEXT_RADIUS: usize = 3;

/// Git's heuristic: a blob is binary if a NUL byte appears early.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// Strip a single trailing line terminator (`\n` or `\r\n`) for display.
fn trim_eol(s: &str) -> &str {
    s.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(s)
}

/// Collapse runs of whitespace and trim. Used both for whitespace-insensitive
/// hashing and for the ignore-whitespace diff mode.
pub fn normalize_line(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Count additions/deletions over the raw (display) diff.
pub fn count_stats(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut adds = 0;
    let mut dels = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => adds += 1,
            ChangeTag::Delete => dels += 1,
            ChangeTag::Equal => {}
        }
    }
    (adds, dels)
}

/// Stable, whitespace-insensitive hash over the canonical diff content
/// (changed lines plus surrounding context), plus the destination path to
/// disambiguate files with otherwise-identical diffs.
///
/// `seahash` is used rather than `DefaultHasher` because the value is persisted
/// to disk and must stay stable across runs and rustc versions.
pub fn content_hash(path: &Path, old: &str, new: &str) -> u64 {
    let old_norm: String = old
        .lines()
        .map(normalize_line)
        .collect::<Vec<_>>()
        .join("\n");
    let new_norm: String = new
        .lines()
        .map(normalize_line)
        .collect::<Vec<_>>()
        .join("\n");

    let mut hasher = seahash::SeaHasher::new();
    hasher.write(path.to_string_lossy().as_bytes());
    hasher.write_u8(0xff);

    let diff = TextDiff::from_lines(&old_norm, &new_norm);
    for group in diff.grouped_ops(CONTEXT_RADIUS) {
        for op in &group {
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Equal => b' ',
                    ChangeTag::Delete => b'-',
                    ChangeTag::Insert => b'+',
                };
                hasher.write_u8(tag);
                hasher.write(trim_eol(change.value()).as_bytes());
                hasher.write_u8(b'\n');
            }
        }
    }
    hasher.finish()
}

/// Build the full diff for a file: the complete line sequence plus hunk
/// (change-cluster) ranges with `context` lines of surrounding context.
///
/// When `ignore_ws`, the comparison is made on whitespace-normalized lines
/// (so whitespace-only edits read as unchanged), while the *displayed* content
/// is still the original text.
pub fn build_file_diff(old: &str, new: &str, context: usize, ignore_ws: bool) -> FileDiff {
    let lines: Vec<DiffLine> = if ignore_ws {
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();
        let old_norm: Vec<String> = old_lines.iter().map(|l| normalize_line(l)).collect();
        let new_norm: Vec<String> = new_lines.iter().map(|l| normalize_line(l)).collect();
        let old_ref: Vec<&str> = old_norm.iter().map(String::as_str).collect();
        let new_ref: Vec<&str> = new_norm.iter().map(String::as_str).collect();
        let diff = TextDiff::from_slices(&old_ref, &new_ref);
        diff.iter_all_changes()
            .map(|change| {
                build_line(
                    change.tag(),
                    change.old_index(),
                    change.new_index(),
                    &old_lines,
                    &new_lines,
                )
            })
            .collect()
    } else {
        let diff = TextDiff::from_lines(old, new);
        diff.iter_all_changes()
            .map(|change| {
                let content = trim_eol(change.value()).to_string();
                match change.tag() {
                    ChangeTag::Equal => DiffLine::Context {
                        old_lineno: change.old_index().unwrap() + 1,
                        new_lineno: change.new_index().unwrap() + 1,
                        content,
                    },
                    ChangeTag::Delete => DiffLine::Removed {
                        old_lineno: change.old_index().unwrap() + 1,
                        content,
                    },
                    ChangeTag::Insert => DiffLine::Added {
                        new_lineno: change.new_index().unwrap() + 1,
                        content,
                    },
                }
            })
            .collect()
    };
    let hunks = compute_hunks(&lines, context);
    FileDiff { lines, hunks }
}

/// Build a `DiffLine` pulling original content by index (used in ignore-ws
/// mode, where the diff was computed on normalized text).
fn build_line(
    tag: ChangeTag,
    old_index: Option<usize>,
    new_index: Option<usize>,
    old_lines: &[&str],
    new_lines: &[&str],
) -> DiffLine {
    match tag {
        ChangeTag::Equal => DiffLine::Context {
            old_lineno: old_index.unwrap() + 1,
            new_lineno: new_index.unwrap() + 1,
            content: new_lines[new_index.unwrap()].to_string(),
        },
        ChangeTag::Delete => DiffLine::Removed {
            old_lineno: old_index.unwrap() + 1,
            content: old_lines[old_index.unwrap()].to_string(),
        },
        ChangeTag::Insert => DiffLine::Added {
            new_lineno: new_index.unwrap() + 1,
            content: new_lines[new_index.unwrap()].to_string(),
        },
    }
}

/// Derive hunks from the full line sequence: each hunk is a maximal run of
/// lines that lies within `context` of some change.
fn compute_hunks(lines: &[DiffLine], context: usize) -> Vec<Hunk> {
    let n = lines.len();
    let mut visible = vec![false; n];
    for (i, line) in lines.iter().enumerate() {
        if line.is_change() {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(n);
            for v in visible[lo..hi].iter_mut() {
                *v = true;
            }
        }
    }

    let mut hunks = Vec::new();
    let mut i = 0;
    while i < n {
        if !visible[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && visible[i] {
            i += 1;
        }
        let range = start..i;
        let slice = &lines[range.clone()];
        let old_start = slice.iter().find_map(DiffLine::old_lineno).unwrap_or(0);
        let new_start = slice.iter().find_map(DiffLine::new_lineno).unwrap_or(0);
        let old_lines = slice.iter().filter(|l| l.old_lineno().is_some()).count();
        let new_lines = slice.iter().filter(|l| l.new_lineno().is_some()).count();
        hunks.push(Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            function_context: None,
            line_range: range,
        });
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn whitespace_only_change_hashes_same() {
        let p = PathBuf::from("a.rs");
        let a = "fn main() {\n    let x = 1;\n}\n";
        let b = "fn main() {\n        let x = 1;\n}\n"; // only indentation changed
        assert_eq!(content_hash(&p, a, a), content_hash(&p, a, b));
    }

    #[test]
    fn meaningful_change_hashes_differently() {
        let p = PathBuf::from("a.rs");
        let a = "let x = 1;\n";
        let b = "let x = 2;\n";
        assert_ne!(content_hash(&p, a, a), content_hash(&p, a, b));
    }

    #[test]
    fn stats_count_changes() {
        let (adds, dels) = count_stats("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!((adds, dels), (2, 1));
    }

    #[test]
    fn hunks_have_correct_line_numbers() {
        let old = "1\n2\n3\n4\n5\n";
        let new = "1\n2\nX\n4\n5\n";
        let fd = build_file_diff(old, new, 3, false);
        assert_eq!(fd.hunks.len(), 1);
        // line 3 changed
        assert!(
            fd.lines
                .iter()
                .any(|l| matches!(l, DiffLine::Removed { old_lineno: 3, .. }))
        );
        assert!(
            fd.lines
                .iter()
                .any(|l| matches!(l, DiffLine::Added { new_lineno: 3, .. }))
        );
    }

    #[test]
    fn distant_changes_form_separate_hunks() {
        // Two changes far apart should not merge into one hunk.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..40 {
            old.push_str(&format!("line{i}\n"));
            new.push_str(&format!("line{i}\n"));
        }
        let mut nv: Vec<&str> = new.lines().collect();
        nv[2] = "CHANGED_A";
        nv[35] = "CHANGED_B";
        let new2 = nv.join("\n") + "\n";
        let fd = build_file_diff(&old, &new2, 3, false);
        assert_eq!(fd.hunks.len(), 2, "distant edits should be two hunks");
        // The full line list retains every line, hunks cover only ~context windows.
        assert!(fd.lines.len() >= 40);
    }
}
