//! Git access layer, built entirely on `gix` (no shelling out).
//!
//! Responsibilities: open/discover the repo, resolve a CLI range spec to a
//! base/head pair, build the [`Changeset`] (file list + stats + commits), and
//! lazily materialize a single file's hunks when it is opened.

pub mod diff;
pub mod model;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::object::tree::diff::ChangeDetached as Change;
use gix::objs::tree::EntryKind;

use model::{Changeset, CommitInfo, DiffLine, FileChange, FileDiff, FileStatus, Hunk, Special};

/// How the CLI argument maps onto a base/head comparison.
#[derive(Clone, Debug)]
pub enum RangeSpec {
    /// Default branch (from remote HEAD, else main/master) vs HEAD, three-dot.
    Default,
    /// `<ref>` vs HEAD, three-dot (PR-review semantics).
    Base(String),
    /// Explicit range. `three_dot` selects merge-base vs direct comparison.
    Range {
        base: String,
        head: String,
        three_dot: bool,
    },
}

impl RangeSpec {
    /// Parse a single CLI argument into a range spec.
    ///
    /// `A..B` is a direct (two-dot) diff; `A...B`, a bare ref, and the empty
    /// arg use three-dot (merge-base) semantics, matching how GitHub renders a
    /// pull request.
    pub fn parse(arg: Option<&str>) -> RangeSpec {
        let Some(arg) = arg else {
            return RangeSpec::Default;
        };
        let arg = arg.trim();
        if arg.is_empty() {
            return RangeSpec::Default;
        }
        if let Some((a, b)) = arg.split_once("...") {
            return RangeSpec::Range {
                base: empty_to_head(a),
                head: empty_to_head(b),
                three_dot: true,
            };
        }
        if let Some((a, b)) = arg.split_once("..") {
            return RangeSpec::Range {
                base: empty_to_head(a),
                head: empty_to_head(b),
                three_dot: false,
            };
        }
        RangeSpec::Base(arg.to_string())
    }
}

fn empty_to_head(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        "HEAD".to_string()
    } else {
        s.to_string()
    }
}

/// A wrapper over `gix::Repository` exposing only what the viewer needs.
pub struct Repo {
    inner: gix::Repository,
    root: PathBuf,
}

impl Repo {
    /// Discover the repository containing `cwd`.
    pub fn discover(cwd: &Path) -> Result<Repo> {
        let mut inner = gix::discover(cwd)
            .map_err(|_| anyhow!("not a git repository (or any parent): {}", cwd.display()))?;
        // A modest object cache makes repeated commit/blob lookups (commit walk,
        // rename detection) noticeably faster.
        inner.object_cache_size_if_unset(16 * 1024 * 1024);
        let root = inner
            .workdir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| inner.git_dir().to_path_buf());
        Ok(Repo { inner, root })
    }

    /// Repository working-tree root (used to anchor config + state files).
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> &Path {
        self.inner.git_dir()
    }

    /// Short name of the currently checked-out branch, if any.
    pub fn current_branch(&self) -> Option<String> {
        self.inner
            .head_name()
            .ok()
            .flatten()
            .map(|n| n.shorten().to_string())
    }

    /// Detect the default branch: prefer the remote's HEAD symbolic target,
    /// then fall back to a local `main`, then `master`. Returns the first
    /// candidate that actually resolves.
    fn default_branch(&self) -> Result<String> {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(Some(r)) = self.inner.try_find_reference("refs/remotes/origin/HEAD")
            && let gix::refs::TargetRef::Symbolic(name) = r.target()
        {
            candidates.push(name.shorten().to_string());
        }
        candidates.push("main".to_string());
        candidates.push("master".to_string());

        for cand in &candidates {
            if self.inner.rev_parse_single(cand.as_str()).is_ok() {
                return Ok(cand.clone());
            }
        }
        Err(anyhow!(
            "could not determine a default branch (tried origin/HEAD, main, master); \
             pass a ref explicitly, e.g. `rudiff <branch>`"
        ))
    }

    /// Resolve a ref/revspec to a commit id, peeling tags as needed.
    fn resolve_commit(&self, spec: &str) -> Result<ObjectId> {
        let id = self
            .inner
            .rev_parse_single(spec)
            .with_context(|| format!("cannot resolve ref `{spec}`"))?;
        let commit = id
            .object()
            .with_context(|| format!("cannot read object for `{spec}`"))?
            .peel_to_commit()
            .with_context(|| format!("`{spec}` does not point at a commit"))?;
        Ok(commit.id)
    }

    /// Resolve a [`RangeSpec`] into commit ids, the effective base tree id
    /// (after applying merge-base for three-dot), and display names.
    fn resolve(&self, spec: &RangeSpec) -> Result<Resolved> {
        let (base_ref, head_ref, three_dot, head_display) = match spec {
            RangeSpec::Default => {
                let base = self.default_branch()?;
                let head_disp = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
                (base, "HEAD".to_string(), true, head_disp)
            }
            RangeSpec::Base(b) => {
                let head_disp = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
                (b.clone(), "HEAD".to_string(), true, head_disp)
            }
            RangeSpec::Range {
                base,
                head,
                three_dot,
            } => (base.clone(), head.clone(), *three_dot, head.clone()),
        };

        let base_commit_id = self.resolve_commit(&base_ref)?;
        let head_commit_id = self.resolve_commit(&head_ref)?;

        // The tree we diff *from*: merge-base for three-dot, the base directly
        // for two-dot. The commit list is the same in both cases (commits on
        // head but not on the original base).
        let base_tree_commit_id = if three_dot {
            self.inner
                .merge_base(base_commit_id, head_commit_id)
                .with_context(|| {
                    format!(
                        "no merge base between `{base_ref}` and `{head_ref}` (unrelated histories?)"
                    )
                })?
                .detach()
        } else {
            base_commit_id
        };

        Ok(Resolved {
            base_tree_commit_id,
            base_commit_id,
            head_commit_id,
            base_name: base_ref,
            head_name: head_display,
        })
    }

    /// Build the full changeset for the given range.
    pub fn build_changeset(&self, spec: &RangeSpec) -> Result<Changeset> {
        let resolved = self.resolve(spec)?;

        let base_tree = self
            .inner
            .find_commit(resolved.base_tree_commit_id)?
            .tree()?;
        let head_tree = self.inner.find_commit(resolved.head_commit_id)?.tree()?;

        // `None` options => configured rename tracking (git default ~50%).
        let changes = self
            .inner
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)
            .context("failed to diff trees")?;

        let mut files: Vec<FileChange> = changes
            .into_iter()
            .filter_map(|c| self.change_to_file(c))
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let commits = self
            .walk_commits(resolved.base_commit_id, resolved.head_commit_id)
            .unwrap_or_default();

        Ok(Changeset {
            base: resolved.base_tree_commit_id,
            head: resolved.head_commit_id,
            base_name: resolved.base_name,
            head_name: resolved.head_name,
            files,
            commits,
            is_working: false,
        })
    }

    /// Build the changeset for uncommitted changes: the working tree vs HEAD,
    /// mirroring `git diff HEAD`. Tracked files with staged and/or unstaged
    /// edits are always included; untracked files (gitignored ones excluded)
    /// are included when `show_untracked`. There is no commit range, so
    /// `commits` is empty.
    pub fn build_working_changeset(&self, show_untracked: bool) -> Result<Changeset> {
        use gix::dir::entry::{Kind, Status};
        use gix::status::Item;
        use gix::status::index_worktree::Item as IwItem;

        let head_id = self
            .resolve_commit("HEAD")
            .context("cannot resolve HEAD (does the branch have any commits yet?)")?;
        let head_tree = self.inner.find_commit(head_id)?.tree()?;

        // Use `git status` only to find *candidate* paths (anything differing
        // across HEAD / index / worktree). We then re-derive each file's net
        // HEAD↔worktree change directly, so a path that is staged and then
        // reverted in the worktree correctly drops out. The bool records
        // whether the path is untracked (a new file not in the index).
        let mut paths: BTreeMap<PathBuf, bool> = BTreeMap::new();
        let untracked_mode = if show_untracked {
            gix::status::UntrackedFiles::Files
        } else {
            gix::status::UntrackedFiles::None
        };
        let iter = self
            .inner
            .status(gix::progress::Discard)
            .context("cannot compute working-tree status")?
            .untracked_files(untracked_mode)
            .into_iter(Vec::<gix::bstr::BString>::new())
            .context("cannot iterate working-tree status")?;
        for item in iter {
            let item = item.context("error reading working-tree status")?;
            match item {
                // Entries surfaced by the directory walk: keep genuinely
                // untracked regular files/symlinks (not ignored, not dirs).
                Item::IndexWorktree(IwItem::DirectoryContents { entry, .. }) => {
                    if entry.status == Status::Untracked
                        && matches!(entry.disk_kind, Some(Kind::File) | Some(Kind::Symlink))
                    {
                        paths.entry(bstr_to_path(entry.rela_path.as_ref())).or_insert(true);
                    }
                }
                // Everything else (HEAD↔index, index↔worktree, rewrites) is a
                // change to a tracked path.
                other => {
                    paths.entry(bstr_to_path(other.location())).or_insert(false);
                }
            }
        }

        let mut files: Vec<FileChange> = paths
            .into_iter()
            .filter_map(|(p, untracked)| self.working_file_change(&head_tree, p, untracked))
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let base_name = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
        Ok(Changeset {
            base: head_id,
            // No distinct head object exists for the working tree; reuse HEAD.
            head: head_id,
            base_name,
            head_name: "working tree".to_string(),
            files,
            commits: Vec::new(),
            is_working: true,
        })
    }

    /// Derive one file's HEAD→worktree change. Old side is the HEAD blob (if
    /// any), new side is the on-disk file (if any). Returns `None` when the two
    /// sides are equal (no net change) or for submodules/trees.
    fn working_file_change(
        &self,
        head_tree: &gix::Tree<'_>,
        path: PathBuf,
        untracked: bool,
    ) -> Option<FileChange> {
        let head_entry = head_tree.lookup_entry_by_path(&path).ok().flatten();
        let head_kind = head_entry.as_ref().map(|e| e.mode().kind());
        // Submodules (gitlinks) and directories are out of scope for this view.
        if matches!(head_kind, Some(EntryKind::Commit) | Some(EntryKind::Tree)) {
            return None;
        }
        let old_id = head_entry.as_ref().map(|e| e.object_id());

        let full = self.root.join(&path);
        let wt_meta = std::fs::symlink_metadata(&full).ok();
        let new_exists = wt_meta.is_some();
        if old_id.is_none() && !new_exists {
            return None; // absent on both sides (e.g. staged add then deleted)
        }

        let status = if old_id.is_none() {
            FileStatus::Added
        } else if !new_exists {
            FileStatus::Deleted
        } else {
            FileStatus::Modified
        };

        let (old_text, old_bin, old_size) = self.read_blob(old_id.as_ref());
        let (new_text, new_bin, new_size) = self.read_worktree(&path);

        let mk = |special, additions, deletions, content_hash| {
            Some(FileChange {
                path: path.clone(),
                old_path: None,
                status,
                additions,
                deletions,
                content_hash,
                special,
                old_id,
                new_id: None,
                new_in_worktree: true,
                untracked,
            })
        };

        if old_bin || new_bin {
            let mut hasher = seahash::SeaHasher::new();
            std::hash::Hasher::write(&mut hasher, path.to_string_lossy().as_bytes());
            if let Some(id) = &old_id {
                std::hash::Hasher::write(&mut hasher, id.as_slice());
            }
            std::hash::Hasher::write(&mut hasher, &new_size.to_le_bytes());
            let h = std::hash::Hasher::finish(&hasher);
            return mk(Special::Binary { old_size, new_size }, 0, 0, h);
        }

        // A modification whose net content equals HEAD (e.g. staged then
        // reverted in the worktree) is not a real change — drop it. An add or
        // delete is always real, even when the present side is empty.
        if status == FileStatus::Modified && old_text == new_text {
            return None;
        }

        let is_symlink = matches!(head_kind, Some(EntryKind::Link))
            || wt_meta.is_some_and(|m| m.file_type().is_symlink());
        let special = if is_symlink {
            Special::Symlink
        } else {
            Special::None
        };
        let (additions, deletions) = diff::count_stats(&old_text, &new_text);
        let content_hash = diff::content_hash(&path, &old_text, &new_text);
        mk(special, additions, deletions, content_hash)
    }

    /// Walk commits reachable from `head` but not from `base`.
    fn walk_commits(&self, base: ObjectId, head: ObjectId) -> Result<Vec<CommitInfo>> {
        let mut out = Vec::new();
        let walk = self
            .inner
            .rev_walk([head])
            .with_hidden([base])
            .all()
            .context("commit walk failed")?;
        for info in walk {
            let info = info?;
            let commit = info.object()?;
            let author_sig = commit.author().ok();
            let author = author_sig
                .map(|a| a.name.trim().to_str_lossy().into_owned())
                .unwrap_or_default();
            // Author time, not committer time, so "branch age" reflects when the
            // work was written. (`info.commit_time()` would panic here because
            // the default topological sort doesn't populate it.)
            let time_secs = author_sig
                .and_then(|a| a.time().ok())
                .map(|t| t.seconds)
                .unwrap_or(0);
            let summary = commit
                .message()
                .map(|m| m.summary().trim().to_str_lossy().into_owned())
                .unwrap_or_default();
            out.push(CommitInfo {
                id: info.id().detach(),
                summary,
                author,
                time_secs,
            });
            // Guard against pathological histories; a PR is never this large.
            if out.len() >= 10_000 {
                break;
            }
        }
        Ok(out)
    }

    /// Convert a gix tree-diff change into our [`FileChange`], computing stats
    /// and the content hash eagerly (both are cheap and the overview needs
    /// them); hunks are deferred to [`Repo::load_file_diff`].
    fn change_to_file(&self, change: Change) -> Option<FileChange> {
        let (status, old_path, path, old_id, new_id, mode) = match change {
            Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => (
                FileStatus::Added,
                None,
                bstr_to_path((location).as_ref()),
                None,
                Some(id),
                entry_mode,
            ),
            Change::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => (
                FileStatus::Deleted,
                None,
                bstr_to_path((location).as_ref()),
                Some(id),
                None,
                entry_mode,
            ),
            Change::Modification {
                location,
                previous_id,
                entry_mode,
                id,
                ..
            } => (
                FileStatus::Modified,
                None,
                bstr_to_path((location).as_ref()),
                Some(previous_id),
                Some(id),
                entry_mode,
            ),
            Change::Rewrite {
                source_location,
                source_id,
                location,
                id,
                entry_mode,
                copy,
                ..
            } => (
                if copy {
                    FileStatus::Copied
                } else {
                    FileStatus::Renamed
                },
                Some(bstr_to_path((source_location).as_ref())),
                bstr_to_path((location).as_ref()),
                Some(source_id),
                Some(id),
                entry_mode,
            ),
        };

        let kind = mode.kind();
        if kind == EntryKind::Tree {
            return None; // directories aren't shown as file changes
        }

        // Submodule: ids point at commits in another repo; never read as blobs.
        if kind == EntryKind::Commit {
            let mut hasher = seahash::SeaHasher::new();
            std::hash::Hasher::write(&mut hasher, path.to_string_lossy().as_bytes());
            if let Some(id) = &old_id {
                std::hash::Hasher::write(&mut hasher, id.as_slice());
            }
            if let Some(id) = &new_id {
                std::hash::Hasher::write(&mut hasher, id.as_slice());
            }
            return Some(FileChange {
                path,
                old_path,
                status,
                additions: 0,
                deletions: 0,
                content_hash: std::hash::Hasher::finish(&hasher),
                special: Special::Submodule,
                old_id,
                new_id,
                new_in_worktree: false,
                untracked: false,
            });
        }

        let (old_text, old_bin, old_size) = self.read_blob(old_id.as_ref());
        let (new_text, new_bin, new_size) = self.read_blob(new_id.as_ref());

        if old_bin || new_bin {
            let mut hasher = seahash::SeaHasher::new();
            std::hash::Hasher::write(&mut hasher, path.to_string_lossy().as_bytes());
            if let Some(id) = &old_id {
                std::hash::Hasher::write(&mut hasher, id.as_slice());
            }
            if let Some(id) = &new_id {
                std::hash::Hasher::write(&mut hasher, id.as_slice());
            }
            return Some(FileChange {
                path,
                old_path,
                status,
                additions: 0,
                deletions: 0,
                content_hash: std::hash::Hasher::finish(&hasher),
                special: Special::Binary { old_size, new_size },
                old_id,
                new_id,
                new_in_worktree: false,
                untracked: false,
            });
        }

        let (additions, deletions) = diff::count_stats(&old_text, &new_text);
        let content_hash = diff::content_hash(&path, &old_text, &new_text);
        let special = if kind == EntryKind::Link {
            Special::Symlink
        } else {
            Special::None
        };

        Some(FileChange {
            path,
            old_path,
            status,
            additions,
            deletions,
            content_hash,
            special,
            old_id,
            new_id,
            new_in_worktree: false,
            untracked: false,
        })
    }

    /// Read a blob's text. Returns `(text, is_binary, size_in_bytes)`.
    fn read_blob(&self, id: Option<&ObjectId>) -> (String, bool, u64) {
        let Some(id) = id else {
            return (String::new(), false, 0);
        };
        match self.inner.find_object(*id) {
            Ok(obj) => {
                let size = obj.data.len() as u64;
                if diff::looks_binary(&obj.data) {
                    (String::new(), true, size)
                } else {
                    (String::from_utf8_lossy(&obj.data).into_owned(), false, size)
                }
            }
            Err(_) => (String::new(), false, 0),
        }
    }

    /// Read a working-tree file's text. Returns `(text, is_binary, size)`.
    /// A symlink yields its target path (matching how git stores link content);
    /// a missing file yields empty/not-binary (the "deleted" side).
    fn read_worktree(&self, path: &Path) -> (String, bool, u64) {
        let full = self.root.join(path);
        match std::fs::symlink_metadata(&full) {
            Ok(md) if md.file_type().is_symlink() => match std::fs::read_link(&full) {
                Ok(target) => {
                    let s = target.to_string_lossy().into_owned();
                    let n = s.len() as u64;
                    (s, false, n)
                }
                Err(_) => (String::new(), false, 0),
            },
            Ok(md) if md.is_file() => match std::fs::read(&full) {
                Ok(bytes) => {
                    let size = bytes.len() as u64;
                    if diff::looks_binary(&bytes) {
                        (String::new(), true, size)
                    } else {
                        (String::from_utf8_lossy(&bytes).into_owned(), false, size)
                    }
                }
                Err(_) => (String::new(), false, 0),
            },
            _ => (String::new(), false, 0),
        }
    }

    /// Read the *new* side of a change: from disk in uncommitted-changes mode,
    /// otherwise from the `new_id` blob.
    fn read_new_side(&self, fc: &FileChange) -> (String, bool, u64) {
        if fc.new_in_worktree {
            self.read_worktree(&fc.path)
        } else {
            self.read_blob(fc.new_id.as_ref())
        }
    }

    /// Materialize the hunks for one file. `context` controls fold radius;
    /// `ignore_ws` re-diffs ignoring whitespace-only changes. Submodule/binary
    /// files return an empty (or synthetic) diff.
    pub fn load_file_diff(&self, fc: &FileChange, context: usize, ignore_ws: bool) -> FileDiff {
        match &fc.special {
            Special::Binary { .. } => FileDiff::default(),
            Special::Submodule => {
                // Mirror git's "Subproject commit <sha>" presentation.
                let mut lines = Vec::new();
                if let Some(id) = &fc.old_id {
                    lines.push(DiffLine::Removed {
                        old_lineno: 1,
                        content: format!("Subproject commit {id}"),
                    });
                }
                if let Some(id) = &fc.new_id {
                    lines.push(DiffLine::Added {
                        new_lineno: 1,
                        content: format!("Subproject commit {id}"),
                    });
                }
                let range = 0..lines.len();
                FileDiff {
                    hunks: vec![Hunk {
                        old_start: 1,
                        old_lines: fc.old_id.is_some() as usize,
                        new_start: 1,
                        new_lines: fc.new_id.is_some() as usize,
                        function_context: None,
                        line_range: range,
                    }],
                    lines,
                }
            }
            Special::None | Special::Symlink => {
                let (old_text, _, _) = self.read_blob(fc.old_id.as_ref());
                let (new_text, _, _) = self.read_new_side(fc);
                diff::build_file_diff(&old_text, &new_text, context, ignore_ws)
            }
        }
    }

    /// Old- and new-side text of a file (empty strings where a side is absent
    /// or the file is binary). Used for syntax highlighting both sides.
    pub fn file_texts(&self, fc: &FileChange) -> (String, String) {
        if fc.is_binary() || fc.special == Special::Submodule {
            return (String::new(), String::new());
        }
        let (old, _, _) = self.read_blob(fc.old_id.as_ref());
        let (new, _, _) = self.read_new_side(fc);
        (old, new)
    }
}

struct Resolved {
    base_tree_commit_id: ObjectId,
    base_commit_id: ObjectId,
    head_commit_id: ObjectId,
    base_name: String,
    head_name: String,
}

/// Convert a git path (bytes) into a `PathBuf`, lossily on non-UTF-8 systems.
fn bstr_to_path(loc: &gix::bstr::BStr) -> PathBuf {
    gix::path::from_bstr(loc).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parsing() {
        assert!(matches!(RangeSpec::parse(None), RangeSpec::Default));
        assert!(matches!(RangeSpec::parse(Some("")), RangeSpec::Default));
        assert!(matches!(RangeSpec::parse(Some("main")), RangeSpec::Base(b) if b == "main"));
        match RangeSpec::parse(Some("a..b")) {
            RangeSpec::Range {
                base,
                head,
                three_dot,
            } => {
                assert_eq!((base.as_str(), head.as_str(), three_dot), ("a", "b", false));
            }
            _ => panic!("expected range"),
        }
        match RangeSpec::parse(Some("a...b")) {
            RangeSpec::Range { three_dot, .. } => assert!(three_dot),
            _ => panic!("expected range"),
        }
        match RangeSpec::parse(Some("a..")) {
            RangeSpec::Range { head, .. } => assert_eq!(head, "HEAD"),
            _ => panic!("expected range"),
        }
    }
}
