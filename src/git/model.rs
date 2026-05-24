//! Core data model for a changeset between two git trees.
//!
//! Shapes follow the handoff sketch but are refined for lazy loading: the
//! overview only needs per-file stats + a content hash, so [`FileChange`]
//! carries the blob ids needed to compute hunks lazily when a file is opened.

use std::path::PathBuf;

use gix::ObjectId;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

impl FileStatus {
    /// Single-letter code shown in the overview/file header.
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
        }
    }

    /// Lowercase annotation word shown in the overview's right column.
    pub fn annotation(self) -> Option<&'static str> {
        match self {
            FileStatus::Added => Some("new"),
            FileStatus::Deleted => Some("deleted"),
            FileStatus::Renamed => Some("renamed"),
            FileStatus::Copied => Some("copied"),
            FileStatus::Modified => None,
        }
    }
}

/// Why a blob couldn't be diffed as text, if applicable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Special {
    #[default]
    None,
    /// Binary content; carries old/new byte sizes for the placeholder message.
    Binary { old_size: u64, new_size: u64 },
    /// Symlink; rendered as a one-line diff of the target path.
    Symlink,
    /// Submodule (gitlink); rendered as a single SHA-change line.
    Submodule,
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: PathBuf,
    /// `Some(_)` for renames/copies (the source path).
    pub old_path: Option<PathBuf>,
    pub status: FileStatus,
    pub additions: usize,
    pub deletions: usize,
    /// Stable hash over the canonical, whitespace-normalized diff content.
    /// Used as the key for viewed-status persistence. See `git::diff`.
    pub content_hash: u64,
    pub special: Special,
    /// Blob ids for lazy hunk loading. `None` when the side is absent
    /// (e.g. `old_id` for an Added file).
    pub old_id: Option<ObjectId>,
    pub new_id: Option<ObjectId>,
}

impl FileChange {
    pub fn is_binary(&self) -> bool {
        matches!(self.special, Special::Binary { .. })
    }
}

#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub summary: String,
    pub author: String,
    /// Author time, seconds since the unix epoch.
    pub time_secs: i64,
}

#[derive(Clone, Debug)]
pub struct Changeset {
    // Resolved endpoint ids; kept for reference/debugging even though the UI
    // works in terms of the file list.
    #[allow(dead_code)]
    pub base: ObjectId,
    #[allow(dead_code)]
    pub head: ObjectId,
    /// Human-readable endpoint names for the header (e.g. "main", "HEAD").
    pub base_name: String,
    pub head_name: String,
    pub files: Vec<FileChange>,
    pub commits: Vec<CommitInfo>,
}

impl Changeset {
    pub fn total_additions(&self) -> usize {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub fn total_deletions(&self) -> usize {
        self.files.iter().map(|f| f.deletions).sum()
    }

    /// Distinct author count across commits in the range.
    pub fn author_count(&self) -> usize {
        let mut authors: Vec<&str> = self.commits.iter().map(|c| c.author.as_str()).collect();
        authors.sort_unstable();
        authors.dedup();
        authors.len()
    }

    /// Age of the branch: author time of the oldest commit unique to the
    /// range (the handoff's "oldest commit not on base").
    pub fn oldest_commit_secs(&self) -> Option<i64> {
        self.commits.iter().map(|c| c.time_secs).min()
    }
}

/// A single rendered line within a hunk.
#[derive(Clone, Debug)]
pub enum DiffLine {
    Context {
        old_lineno: usize,
        new_lineno: usize,
        content: String,
    },
    Removed {
        old_lineno: usize,
        content: String,
    },
    Added {
        new_lineno: usize,
        content: String,
    },
}

impl DiffLine {
    pub fn content(&self) -> &str {
        match self {
            DiffLine::Context { content, .. }
            | DiffLine::Removed { content, .. }
            | DiffLine::Added { content, .. } => content,
        }
    }

    pub fn is_change(&self) -> bool {
        !matches!(self, DiffLine::Context { .. })
    }

    /// The line number on the old side, if this line exists there.
    pub fn old_lineno(&self) -> Option<usize> {
        match self {
            DiffLine::Context { old_lineno, .. } | DiffLine::Removed { old_lineno, .. } => {
                Some(*old_lineno)
            }
            DiffLine::Added { .. } => None,
        }
    }

    /// The line number on the new side, if this line exists there.
    pub fn new_lineno(&self) -> Option<usize> {
        match self {
            DiffLine::Context { new_lineno, .. } | DiffLine::Added { new_lineno, .. } => {
                Some(*new_lineno)
            }
            DiffLine::Removed { .. } => None,
        }
    }
}

/// A cluster of changes plus its surrounding default context. `line_range`
/// indexes into [`FileDiff::lines`]; the lines outside any hunk's range are the
/// foldable unchanged regions between hunks.
#[derive(Clone, Debug)]
pub struct Hunk {
    // Traditional `@@ -old_start,old_lines +new_start,new_lines @@` header data.
    // We display "hunk N of M" + function context instead, so only `new_start`
    // is read today; the rest is kept to model a hunk faithfully.
    #[allow(dead_code)]
    pub old_start: usize,
    #[allow(dead_code)]
    pub old_lines: usize,
    pub new_start: usize,
    #[allow(dead_code)]
    pub new_lines: usize,
    /// Enclosing function/type context from tree-sitter (populated lazily).
    pub function_context: Option<String>,
    pub line_range: std::ops::Range<usize>,
}

/// The fully-parsed diff for a single file, computed lazily on open.
///
/// `lines` is the complete change sequence (all context included) so that
/// folds can be expanded/collapsed purely as a display concern. `hunks` marks
/// the change clusters (with default context) within `lines`.
#[derive(Clone, Debug, Default)]
pub struct FileDiff {
    pub lines: Vec<DiffLine>,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// 0-based new-file row to anchor a hunk's function-context label on: the
    /// context line immediately preceding the hunk's first change (which sits
    /// *inside* the enclosing function), falling back to the first change or
    /// the hunk start. This yields e.g. `impl Store::refresh` rather than just
    /// `impl Store` when the change is deep in a method body.
    pub fn hunk_context_row(&self, hunk: &Hunk) -> usize {
        let mut last_new: Option<usize> = None;
        for i in hunk.line_range.clone() {
            let line = &self.lines[i];
            if line.is_change() {
                if let Some(n) = last_new {
                    return n.saturating_sub(1);
                }
                if let Some(n) = line.new_lineno() {
                    return n.saturating_sub(1);
                }
                break;
            }
            if let Some(n) = line.new_lineno() {
                last_new = Some(n);
            }
        }
        hunk.new_start.saturating_sub(1)
    }
}
