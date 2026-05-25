//! Committing the reviewed files from the uncommitted-changes view: collect a
//! message, then stage and commit exactly those paths.
//!
//! Like the explain feature (which shells out to `claude`), this shells out to
//! `git` rather than going through the gix-based [`crate::git::Repo`].
//! Committing touches the index, pre-commit hooks, GPG signing, and the user's
//! commit config — all of which the `git` binary handles correctly and for
//! free. This is the one place rudiff writes to the repository, and only on an
//! explicit keypress in `--uncommitted` mode.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// State of the "commit reviewed files" overlay.
pub enum Commit {
    Idle,
    /// Collecting the commit message for the captured set of files.
    Prompting(Prompting),
}

pub struct Prompting {
    /// The commit message being typed.
    pub input: String,
    /// Working-tree paths to stage and commit (the reviewed files), in display
    /// order. Captured when the prompt opens so the list can't shift underfoot.
    pub files: Vec<PathBuf>,
    /// Inline validation/error notice (empty message, git failure); cleared on
    /// the next keystroke so the user can fix and retry.
    pub notice: Option<String>,
}

impl Commit {
    /// Open the message prompt for a captured set of files.
    pub fn prompt(files: Vec<PathBuf>) -> Commit {
        Commit::Prompting(Prompting {
            input: String::new(),
            files,
            notice: None,
        })
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Commit::Idle)
    }

    /// True when the overlay should be shown (and should swallow keys).
    pub fn is_active(&self) -> bool {
        !self.is_idle()
    }

    pub fn is_prompting(&self) -> bool {
        matches!(self, Commit::Prompting(_))
    }

    pub fn input_push(&mut self, c: char) {
        if let Commit::Prompting(p) = self {
            p.input.push(c);
            p.notice = None;
        }
    }

    pub fn input_backspace(&mut self) {
        if let Commit::Prompting(p) = self {
            p.input.pop();
            p.notice = None;
        }
    }

    /// Show an inline notice without closing the prompt (validation/git error).
    pub fn set_notice(&mut self, msg: String) {
        if let Commit::Prompting(p) = self {
            p.notice = Some(msg);
        }
    }
}

/// Stage and commit exactly `files` with `message`, run with `root` as the
/// working directory. Scoping both the `add` and the `commit` to these paths
/// means anything else the user had staged is left untouched. Returns a short
/// human-readable summary (the new commit's short hash + subject) on success,
/// or a one-line error message on failure.
pub fn run_commit(root: &Path, files: &[PathBuf], message: &str) -> Result<String, String> {
    // Stage the reviewed paths. A single `git add -- <paths>` covers additions,
    // edits, and deletions (a removed file is staged as a deletion), and brings
    // any untracked files into the index so the scoped commit below sees them.
    let add = Command::new("git")
        .current_dir(root)
        .arg("add")
        .arg("--")
        .args(files)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !add.status.success() {
        return Err(git_error("git add", &add));
    }

    // Commit only those paths (a partial commit), so any unrelated staged work
    // is preserved rather than swept into this commit.
    let commit = Command::new("git")
        .current_dir(root)
        .arg("commit")
        .arg("-m")
        .arg(message)
        .arg("--")
        .args(files)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !commit.status.success() {
        return Err(git_error("git commit", &commit));
    }

    Ok(head_summary(root))
}

/// `<short-hash> <subject>` for the just-created HEAD commit; a plain fallback
/// if the lookup fails (the commit itself already succeeded).
fn head_summary(root: &Path) -> String {
    Command::new("git")
        .current_dir(root)
        .args(["log", "-1", "--pretty=%h %s"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "commit created".to_string())
}

/// Condense a failed git invocation into a single-line message for the footer
/// notice: prefer stderr, fall back to stdout, then the exit code.
fn git_error(what: &str, out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let detail = first_line(stderr.trim())
        .or_else(|| first_line(stdout.trim()))
        .unwrap_or("");
    if detail.is_empty() {
        format!("{what} failed (exit {})", out.status.code().unwrap_or(-1))
    } else {
        format!("{what}: {detail}")
    }
}

/// First non-empty line of `s`, trimmed; `None` if there is none.
fn first_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).find(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `git` with `root` as the cwd, panicking on failure (test helper).
    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn commits_only_the_named_paths() {
        let dir = std::env::temp_dir().join(format!("rudiff-commit-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.as_path();

        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("tracked.txt"), "a\n").unwrap();
        std::fs::write(root.join("gone.txt"), "old\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "init"]);

        // Working state: edit one tracked file, delete another, add a new file,
        // and touch an unrelated file that is NOT in the reviewed set.
        std::fs::write(root.join("tracked.txt"), "a\nb\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        std::fs::write(root.join("unrelated.txt"), "noise\n").unwrap();

        let files = vec![
            PathBuf::from("tracked.txt"),
            PathBuf::from("gone.txt"),
            PathBuf::from("new.txt"),
        ];
        let summary = run_commit(root, &files, "review batch").unwrap();
        assert!(summary.contains("review batch"), "summary was: {summary}");

        // The named paths are committed (the modify/delete/add all landed)…
        let names = Command::new("git")
            .current_dir(root)
            .args(["show", "--name-status", "--pretty=format:", "HEAD"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&names.stdout);
        assert!(names.contains("tracked.txt"));
        assert!(names.contains("gone.txt"));
        assert!(names.contains("new.txt"));
        assert!(!names.contains("unrelated.txt"));

        // …and the unrelated file is still an untracked working-tree change.
        let status = Command::new("git")
            .current_dir(root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(status.contains("unrelated.txt"), "status was: {status}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn first_line_picks_first_nonempty() {
        assert_eq!(first_line("\n\n  hello \nworld"), Some("hello"));
        assert_eq!(first_line("   "), None);
        assert_eq!(first_line(""), None);
    }
}
