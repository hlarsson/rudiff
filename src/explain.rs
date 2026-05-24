//! "Explain these changes" — shells out to the Claude Code CLI (`claude -p`)
//! to summarize a diff, streaming the response as it arrives without blocking
//! or freezing the TUI.
//!
//! We run `claude` with `--output-format stream-json --include-partial-messages`
//! so it emits newline-delimited JSON events; a reader thread parses each line
//! and forwards the text deltas over a channel. The [`Child`] handle stays here
//! so the query can be killed mid-flight, and the event loop calls
//! [`Explain::poll`] each tick to append new text and detect completion.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::Instant;

use crate::git::model::{DiffLine, FileDiff};

/// Cap the diff we hand to `claude` so we stay well under arg-size limits and
/// keep the request cheap; an explanation doesn't need every last line.
pub const MAX_DIFF_BYTES: usize = 60 * 1024;

/// Which Claude model to use for `e`, configurable via `.rudiff.toml`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExplainModel {
    Haiku,
    Sonnet,
    Opus,
}

impl ExplainModel {
    /// Parse a config value (case-insensitive). `None` for unrecognized names.
    pub fn from_name(s: &str) -> Option<ExplainModel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "haiku" => Some(ExplainModel::Haiku),
            "sonnet" => Some(ExplainModel::Sonnet),
            "opus" => Some(ExplainModel::Opus),
            _ => None,
        }
    }

    /// The alias passed to `claude --model` (also used as the display label).
    pub fn alias(self) -> &'static str {
        match self {
            ExplainModel::Haiku => "haiku",
            ExplainModel::Sonnet => "sonnet",
            ExplainModel::Opus => "opus",
        }
    }
}

/// State of the explain overlay.
pub enum Explain {
    Idle,
    /// Collecting optional extra guidance from the reviewer before asking.
    Prompting(Prompting),
    Running(Running),
    /// Finished: the text to display (an answer, or an error message).
    Result {
        text: String,
        is_error: bool,
        /// Scroll offset (in wrapped lines) within the result overlay.
        scroll: usize,
        /// What was explained — used for the suggested save filename + header.
        target: String,
        /// `Some(filename)` while the user is editing the save path.
        save: Option<String>,
        /// Transient confirmation/error shown in the footer (e.g. "Saved to …").
        notice: Option<String>,
    },
}

/// Captured request plus the guidance line the user is typing.
pub struct Prompting {
    /// Free-text guidance being entered (may be empty).
    pub input: String,
    /// What we're explaining, for the popup label.
    pub target: String,
    /// Model to use (from config); `None` uses `claude`'s default.
    pub model: Option<ExplainModel>,
    /// Base instruction; the guidance is appended on submit.
    instruction: String,
    /// The diff to explain, rendered once when `e` was pressed.
    diff_text: String,
}

pub struct Running {
    child: Child,
    rx: Receiver<Msg>,
    /// Response text accumulated so far, shown live as it streams in.
    pub partial: String,
    /// Captured stderr (shown only if the run fails).
    stderr: String,
    /// Authoritative final text + error flag from the terminal `result` event.
    result: Option<(String, bool)>,
    /// Whether stdout / stderr have hit EOF (the process is done once both have).
    stdout_done: bool,
    stderr_done: bool,
    /// What we're explaining, for the overlay label (e.g. a file path).
    pub target: String,
    /// Model in use, for the overlay label.
    pub model: Option<ExplainModel>,
    pub started: Instant,
}

enum Msg {
    /// An incremental text delta from the model.
    Chunk(String),
    /// The terminal `result` event: full text and whether it was an error.
    Result {
        text: Option<String>,
        is_error: bool,
    },
    /// stdout reached EOF (the model finished or was killed).
    OutDone,
    /// stderr contents (sent once, at EOF).
    Err(String),
}

impl Explain {
    /// Kick off `claude -p` to explain `diff_text`. The instruction is the
    /// prompt; the diff is appended to it. Failure to even spawn (e.g. `claude`
    /// not on PATH) lands directly in a `Result` error state.
    pub fn start(
        instruction: &str,
        diff_text: &str,
        target: String,
        model: Option<ExplainModel>,
    ) -> Explain {
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

        match spawn(&prompt, model) {
            Ok((child, rx)) => Explain::Running(Running {
                child,
                rx,
                partial: String::new(),
                stderr: String::new(),
                result: None,
                stdout_done: false,
                stderr_done: false,
                target,
                model,
                started: Instant::now(),
            }),
            Err(e) => Explain::Result {
                text: format!(
                    "Couldn't run `claude`: {e}\n\n\
                     The Claude Code CLI must be installed and on your PATH for this to work."
                ),
                is_error: true,
                scroll: 0,
                target,
                save: None,
                notice: None,
            },
        }
    }

    /// Open the guidance popup for a captured request (does not spawn yet).
    pub fn prompt(
        instruction: String,
        diff_text: String,
        target: String,
        model: Option<ExplainModel>,
    ) -> Explain {
        Explain::Prompting(Prompting {
            input: String::new(),
            target,
            model,
            instruction,
            diff_text,
        })
    }

    /// Submit the guidance popup: fold any guidance into the instruction and
    /// fire the query. No-op unless we're in the `Prompting` state.
    pub fn submit(&mut self) {
        if let Explain::Prompting(p) = std::mem::replace(self, Explain::Idle) {
            let mut instruction = p.instruction;
            let guidance = p.input.trim();
            if !guidance.is_empty() {
                instruction
                    .push_str("\n\nThe reviewer specifically asks you to focus on / answer this: ");
                instruction.push_str(guidance);
            }
            *self = Explain::start(&instruction, &p.diff_text, p.target, p.model);
        }
    }

    pub fn input_push(&mut self, c: char) {
        if let Explain::Prompting(p) = self {
            p.input.push(c);
        }
    }

    pub fn input_backspace(&mut self) {
        if let Explain::Prompting(p) = self {
            p.input.pop();
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Explain::Idle)
    }

    pub fn is_prompting(&self) -> bool {
        matches!(self, Explain::Prompting(_))
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Explain::Running(_))
    }

    /// True when an overlay (prompt, spinner, or result) should be shown.
    pub fn is_active(&self) -> bool {
        !self.is_idle()
    }

    /// Append any newly-streamed text from the worker threads; once both
    /// streams have hit EOF (the process exited or was killed), reap it and
    /// transition to `Result`.
    pub fn poll(&mut self) {
        let Explain::Running(r) = self else { return };
        while let Ok(msg) = r.rx.try_recv() {
            match msg {
                Msg::Chunk(s) => r.partial.push_str(&s),
                Msg::Result { text, is_error } => {
                    r.result = Some((text.unwrap_or_default(), is_error));
                }
                Msg::OutDone => r.stdout_done = true,
                Msg::Err(s) => {
                    r.stderr = s;
                    r.stderr_done = true;
                }
            }
        }
        if !(r.stdout_done && r.stderr_done) {
            return;
        }
        let success = r.child.wait().ok().map(|s| s.success()).unwrap_or(false);
        let target = std::mem::take(&mut r.target);

        // Prefer the authoritative `result` text; fall back to the streamed
        // partial. Surface stderr (or a generic note) when things went wrong.
        let result_text = r.result.as_ref().map(|(t, _)| t.trim()).unwrap_or("");
        let result_error = r.result.as_ref().map(|(_, e)| *e).unwrap_or(false);
        let streamed = r.partial.trim();

        let (text, is_error) = if !result_error && !result_text.is_empty() {
            (result_text.to_string(), false)
        } else if !result_error && !streamed.is_empty() {
            (streamed.to_string(), false)
        } else if !r.stderr.trim().is_empty() {
            (
                format!("`claude` reported an error:\n\n{}", r.stderr.trim()),
                true,
            )
        } else if result_error && !result_text.is_empty() {
            (result_text.to_string(), true)
        } else if !streamed.is_empty() {
            // Killed (cancel) or no result event, but we have partial text.
            (streamed.to_string(), success)
        } else {
            ("`claude` returned no output.".to_string(), true)
        };
        *self = Explain::Result {
            text,
            is_error,
            scroll: 0,
            target,
            save: None,
            notice: None,
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
        if let Explain::Result {
            scroll,
            save,
            notice,
            ..
        } = self
        {
            if save.is_some() {
                return; // editing the filename — don't scroll
            }
            *notice = None; // any navigation clears a stale "saved" notice
            *scroll = (*scroll as isize + delta).max(0) as usize;
        }
    }

    // ---- Saving the response to a Markdown file ----

    pub fn is_saving(&self) -> bool {
        matches!(self, Explain::Result { save: Some(_), .. })
    }

    /// Begin saving: open the filename field pre-filled with a suggestion.
    pub fn start_save(&mut self) {
        if let Explain::Result {
            target,
            save,
            notice,
            ..
        } = self
            && save.is_none()
        {
            *save = Some(suggested_filename(target));
            *notice = None;
        }
    }

    pub fn save_input_push(&mut self, c: char) {
        if let Explain::Result {
            save: Some(name), ..
        } = self
        {
            name.push(c);
        }
    }

    pub fn save_input_backspace(&mut self) {
        if let Explain::Result {
            save: Some(name), ..
        } = self
        {
            name.pop();
        }
    }

    /// Cancel the filename editor, returning to the response view.
    pub fn cancel_save(&mut self) {
        if let Explain::Result { save, .. } = self {
            *save = None;
        }
    }

    /// Write the response to the entered filename (resolved against `cwd`).
    /// Reports success/failure via the `notice` field; on success the editor
    /// closes, on failure it stays open so the user can fix the path.
    pub fn confirm_save(&mut self, cwd: &std::path::Path) {
        let Explain::Result {
            text,
            target,
            save,
            notice,
            ..
        } = self
        else {
            return;
        };
        let Some(name) = save.as_ref().map(|s| s.trim().to_string()) else {
            return;
        };
        if name.is_empty() {
            *notice = Some("Enter a filename".to_string());
            return;
        }
        // An absolute path replaces `cwd`; a relative one is joined onto it.
        let path = cwd.join(&name);
        let contents = format!("# rudiff explanation — {target}\n\n{text}\n");
        match std::fs::write(&path, contents) {
            Ok(()) => {
                *save = None;
                *notice = Some(format!("Saved to {}", path.display()));
            }
            Err(e) => *notice = Some(format!("Save failed: {e}")),
        }
    }
}

/// A descriptive, conflict-resistant default filename: the target slug plus a
/// UTC timestamp to the second.
fn suggested_filename(target: &str) -> String {
    format!("rudiff-explain-{}-{}.md", slug(target), timestamp())
}

/// Turn a target like `src/session.rs` into a filename-safe slug.
fn slug(target: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for ch in target.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    let s = s.trim_matches('-');
    let slug: String = s.chars().take(40).collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "diff".to_string()
    } else {
        slug
    }
}

/// `YYYYMMDD-HHMMSS` in UTC, computed without a date crate.
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

/// Convert days-since-Unix-epoch to (year, month, day). Howard Hinnant's
/// `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn spawn(prompt: &str, model: Option<ExplainModel>) -> std::io::Result<(Child, Receiver<Msg>)> {
    // `--output-format stream-json` streams newline-delimited events as they
    // arrive; it requires `--verbose` in print mode. `--include-partial-messages`
    // gives us token-level text deltas rather than whole messages.
    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(model) = model {
        cmd.arg("--model").arg(model.alias());
    }
    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = channel();

    // stdout: parse each JSON line, forward text deltas and the final result.
    let tx_out = tx.clone();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Some(msg) = parse_event(&line)
                && tx_out.send(msg).is_err()
            {
                break; // receiver gone (cancelled / quit)
            }
        }
        let _ = tx_out.send(Msg::OutDone);
    });
    // stderr: collected whole; only surfaced if the run fails.
    thread::spawn(move || {
        let mut buf = String::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_string(&mut buf);
        let _ = tx.send(Msg::Err(buf));
    });

    Ok((child, rx))
}

/// Translate one stream-json line into a [`Msg`], if it carries text or the
/// final result. Unknown / structural events yield `None`.
fn parse_event(line: &str) -> Option<Msg> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    match v["type"].as_str()? {
        "stream_event" => {
            let event = &v["event"];
            if event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta" {
                let text = event["delta"]["text"].as_str()?;
                Some(Msg::Chunk(text.to_string()))
            } else {
                None
            }
        }
        "result" => Some(Msg::Result {
            text: v["result"].as_str().map(str::to_string),
            is_error: v["is_error"].as_bool().unwrap_or(false),
        }),
        _ => None,
    }
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
    fn parses_text_delta_and_result_events() {
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#;
        match parse_event(delta) {
            Some(Msg::Chunk(s)) => assert_eq!(s, "Hello"),
            _ => panic!("expected a text chunk"),
        }

        let result =
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello world"}"#;
        match parse_event(result) {
            Some(Msg::Result { text, is_error }) => {
                assert_eq!(text.as_deref(), Some("Hello world"));
                assert!(!is_error);
            }
            _ => panic!("expected a result"),
        }

        // Structural / unrelated events produce nothing.
        assert!(parse_event(r#"{"type":"system","subtype":"init"}"#).is_none());
        assert!(parse_event("not json").is_none());
    }

    #[test]
    fn slug_and_timestamp_are_sane() {
        assert_eq!(slug("src/session.rs"), "src-session-rs");
        assert_eq!(slug("feat (12 files)"), "feat-12-files");
        assert_eq!(slug("!!!"), "diff"); // no usable chars
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        let ts = timestamp();
        assert_eq!(ts.len(), 15); // YYYYMMDD-HHMMSS
        assert!(ts.as_bytes()[8] == b'-');
    }

    #[test]
    fn confirm_save_writes_the_response() {
        let dir = std::env::temp_dir().join(format!("rudiff-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut e = Explain::Result {
            text: "# Title\n\n- a point".to_string(),
            is_error: false,
            scroll: 0,
            target: "src/foo.rs".to_string(),
            save: Some("out.md".to_string()),
            notice: None,
        };
        e.confirm_save(&dir);
        let written = std::fs::read_to_string(dir.join("out.md")).unwrap();
        assert!(written.contains("# rudiff explanation — src/foo.rs"));
        assert!(written.contains("- a point"));
        // After a successful save we leave the editor and report it.
        assert!(!e.is_saving());
        match &e {
            Explain::Result {
                notice: Some(n), ..
            } => assert!(n.starts_with("Saved to")),
            _ => panic!("expected a result with a notice"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_claude_yields_error_result() {
        // Spawning a bogus binary should land in an error Result, not panic.
        let mut e = Explain::start("explain", "diff", "x".into(), None);
        // If `claude` happens to exist we'll get Running; otherwise an error.
        // Either way it must be active and must not panic when polled.
        e.poll();
        assert!(e.is_active());
        e.cancel();
        assert!(e.is_idle());
    }
}
