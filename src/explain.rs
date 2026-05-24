//! "Explain these changes" — shells out to the Claude Code CLI (`claude -p`)
//! to summarize a diff, without blocking or freezing the TUI.
//!
//! The child runs with piped stdio; dedicated threads drain stdout/stderr so we
//! never deadlock on a full pipe, and the [`Child`] handle stays here so the
//! query can be killed mid-flight. The event loop calls [`Explain::poll`] each
//! tick to pick up completion.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::Instant;

use crate::git::model::{DiffLine, FileDiff};

/// Cap the diff we hand to `claude` so we stay well under arg-size limits and
/// keep the request cheap; an explanation doesn't need every last line.
pub const MAX_DIFF_BYTES: usize = 60 * 1024;

/// State of the explain overlay.
pub enum Explain {
    Idle,
    Running(Running),
    /// Finished: the text to display (an answer, or an error message).
    Result {
        text: String,
        is_error: bool,
        /// Scroll offset (in wrapped lines) within the result overlay.
        scroll: usize,
    },
}

pub struct Running {
    child: Child,
    rx: Receiver<Msg>,
    out: Option<String>,
    err: Option<String>,
    /// What we're explaining, for the spinner label (e.g. a file path).
    pub target: String,
    pub started: Instant,
}

enum Msg {
    Out(String),
    Err(String),
}

impl Explain {
    /// Kick off `claude -p` to explain `diff_text`. The instruction is the
    /// prompt; the diff is appended to it. Failure to even spawn (e.g. `claude`
    /// not on PATH) lands directly in a `Result` error state.
    pub fn start(instruction: &str, diff_text: &str, target: String) -> Explain {
        let mut prompt = String::with_capacity(instruction.len() + diff_text.len() + 16);
        prompt.push_str(instruction);
        prompt.push_str("\n\n```diff\n");
        if diff_text.len() > MAX_DIFF_BYTES {
            // Cut on a line boundary near the cap.
            let cut = diff_text[..MAX_DIFF_BYTES]
                .rfind('\n')
                .unwrap_or(MAX_DIFF_BYTES);
            prompt.push_str(&diff_text[..cut]);
            prompt.push_str("\n… (diff truncated)\n");
        } else {
            prompt.push_str(diff_text);
        }
        prompt.push_str("```\n");

        match spawn(&prompt) {
            Ok((child, rx)) => Explain::Running(Running {
                child,
                rx,
                out: None,
                err: None,
                target,
                started: Instant::now(),
            }),
            Err(e) => Explain::Result {
                text: format!(
                    "Couldn't run `claude`: {e}\n\n\
                     The Claude Code CLI must be installed and on your PATH for this to work."
                ),
                is_error: true,
                scroll: 0,
            },
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Explain::Idle)
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Explain::Running(_))
    }

    /// True when an overlay (spinner or result) should be shown.
    pub fn is_active(&self) -> bool {
        !self.is_idle()
    }

    /// Drain any output from the worker threads; once both streams have hit EOF
    /// (the process exited or was killed), reap it and transition to `Result`.
    pub fn poll(&mut self) {
        let Explain::Running(r) = self else { return };
        while let Ok(msg) = r.rx.try_recv() {
            match msg {
                Msg::Out(s) => r.out = Some(s),
                Msg::Err(s) => r.err = Some(s),
            }
        }
        if r.out.is_none() || r.err.is_none() {
            return;
        }
        let status = r.child.wait().ok();
        let out = r.out.take().unwrap_or_default();
        let err = r.err.take().unwrap_or_default();
        let success = status.map(|s| s.success()).unwrap_or(false);

        let (text, is_error) = if success && !out.trim().is_empty() {
            (out.trim().to_string(), false)
        } else if !err.trim().is_empty() {
            (
                format!("`claude` reported an error:\n\n{}", err.trim()),
                true,
            )
        } else if !out.trim().is_empty() {
            (out.trim().to_string(), false)
        } else {
            ("`claude` returned no output.".to_string(), true)
        };
        *self = Explain::Result {
            text,
            is_error,
            scroll: 0,
        };
    }

    /// Kill an in-flight query and return to idle. No-op otherwise.
    pub fn cancel(&mut self) {
        if let Explain::Running(r) = self {
            let _ = r.child.kill();
            let _ = r.child.wait(); // reap; the reader threads then exit on EOF
        }
        *self = Explain::Idle;
    }

    pub fn dismiss(&mut self) {
        *self = Explain::Idle;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        if let Explain::Result { scroll, .. } = self {
            *scroll = (*scroll as isize + delta).max(0) as usize;
        }
    }
}

fn spawn(prompt: &str) -> std::io::Result<(Child, Receiver<Msg>)> {
    let mut child = Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = channel();

    let tx_out = tx.clone();
    thread::spawn(move || {
        let mut buf = String::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx_out.send(Msg::Out(buf));
    });
    thread::spawn(move || {
        let mut buf = String::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_string(&mut buf);
        let _ = tx.send(Msg::Err(buf));
    });

    Ok((child, rx))
}

/// Render a file's diff as plain `git diff`-style unified text for the prompt.
pub fn render_unified(path: &Path, fd: &FileDiff) -> String {
    let p = path.display();
    let mut s = format!("--- a/{p}\n+++ b/{p}\n");
    for h in &fd.hunks {
        s.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        ));
        for idx in h.line_range.clone() {
            let (prefix, content) = match &fd.lines[idx] {
                DiffLine::Context { content, .. } => (' ', content),
                DiffLine::Removed { content, .. } => ('-', content),
                DiffLine::Added { content, .. } => ('+', content),
            };
            s.push(prefix);
            s.push_str(content);
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_unified_produces_diff_text() {
        let fd = crate::git::diff::build_file_diff("a\nb\nc\n", "a\nB\nc\n", 3, false);
        let text = render_unified(Path::new("x.rs"), &fd);
        assert!(text.contains("--- a/x.rs"));
        assert!(text.contains("+++ b/x.rs"));
        assert!(text.contains("@@ "));
        assert!(text.contains("-b"));
        assert!(text.contains("+B"));
        assert!(text.contains(" a")); // context line
    }

    #[test]
    fn missing_claude_yields_error_result() {
        // Spawning a bogus binary should land in an error Result, not panic.
        let mut e = Explain::start("explain", "diff", "x".into());
        // If `claude` happens to exist we'll get Running; otherwise an error.
        // Either way it must be active and must not panic when polled.
        e.poll();
        assert!(e.is_active());
        e.cancel();
        assert!(e.is_idle());
    }
}
