//! Application state and the main event loop.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::cli::Cli;
use crate::config::Config;
use crate::git::Repo;
use crate::git::model::Changeset;
use crate::group::{self, Grouping};
use crate::term::Tui;
use crate::theme::Theme;
use crate::ui;
use crate::viewed::Viewed;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Overview,
    Diff,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Unified,
    SideBySide,
}

/// Whitespace handling in the diff body: `Show` renders changes normally,
/// `Dim` de-emphasizes whitespace-only changed lines, `Ignore` re-diffs so they
/// disappear entirely.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Whitespace {
    Show,
    Dim,
    Ignore,
}

impl Whitespace {
    pub fn label(self) -> &'static str {
        match self {
            Whitespace::Show => "show",
            Whitespace::Dim => "dim",
            Whitespace::Ignore => "ignore",
        }
    }
    fn next(self) -> Whitespace {
        match self {
            Whitespace::Show => Whitespace::Dim,
            Whitespace::Dim => Whitespace::Ignore,
            Whitespace::Ignore => Whitespace::Show,
        }
    }
}

/// Width (in columns) at or above which side-by-side is the default layout.
pub const SIDE_BY_SIDE_MIN_WIDTH: u16 = 165;

/// One rendered row of the diff body. Indices reference `DiffState::doc`.
#[derive(Clone)]
pub enum DisplayRow {
    /// Separator before hunk `doc.hunks[hunk]`.
    HunkSep { hunk: usize },
    /// A collapsed unchanged region of `hidden` lines starting at line `start`.
    Fold { start: usize, hidden: usize },
    /// Unified mode: a real diff line (index into `doc.lines`).
    Line { idx: usize },
    /// Side-by-side mode: an old-side line and/or a new-side line that share a
    /// visual row. Either may be `None` (empty half). For a context line both
    /// point at the same `doc.lines` index.
    SideLine {
        left: Option<usize>,
        right: Option<usize>,
    },
}

/// Diff-screen state for the currently-open file.
pub struct DiffState {
    /// First visible display row (scroll position).
    pub scroll: usize,
    /// The loaded diff and which changeset file it belongs to.
    pub doc: crate::git::model::FileDiff,
    pub file_index: usize,
    /// Fold gaps the user has expanded, keyed by their starting line index.
    pub expanded: HashSet<usize>,
    /// Flattened display rows (rebuilt when doc/folds change).
    pub rows: Vec<DisplayRow>,
    /// Display-row index of each hunk's separator (aligned to `doc.hunks`).
    pub sep_rows: Vec<usize>,
    pub viewport_rows: usize,
    /// Total display rows, for clamping motions.
    pub total_rows: usize,
    pub mode: ViewMode,
    pub whitespace: Whitespace,
    /// Per-source-line syntax highlight segments for the old/new sides.
    /// `None` when the language is unknown or highlighting was skipped.
    pub old_hl: Option<Vec<Vec<crate::syntax::Seg>>>,
    pub new_hl: Option<Vec<Vec<crate::syntax::Seg>>>,
    /// Files elsewhere in the changeset that reference this file's symbols.
    pub related: Vec<RelatedEntry>,
    /// For paired -/+ lines, the char indices that changed within each line
    /// (keyed by `doc.lines` index). Drives intra-line highlighting.
    pub intra: std::collections::HashMap<usize, std::collections::HashSet<usize>>,
    /// Line indices whose change is whitespace-only (for `dim` mode).
    pub ws_only: std::collections::HashSet<usize>,
    /// Active in-file search query (empty = no search).
    pub search: String,
    /// Display-row indices of rows matching the search, in order.
    pub matches: Vec<usize>,
    /// Index into `matches` of the current match.
    pub match_idx: usize,
}

/// An entry in the "Related in this PR" panel.
#[derive(Clone)]
pub struct RelatedEntry {
    pub file_index: usize,
    pub verb: crate::syntax::Verb,
    /// Number of this file's symbols the other file references (rank key).
    pub count: usize,
    /// A representative matched symbol, for display.
    pub sample: String,
}

impl DiffState {
    /// Recompute `rows` from the doc, the set of expanded folds, and the
    /// current view mode (unified vs side-by-side produce different rows
    /// because side-by-side merges paired -/+ lines onto one row).
    pub fn rebuild_rows(&mut self) {
        let side = self.mode == ViewMode::SideBySide;
        let mut rows = Vec::new();
        let mut sep_rows = Vec::new();
        let n = self.doc.lines.len();
        let mut cursor = 0;

        let emit_context = |rows: &mut Vec<DisplayRow>, idx: usize| {
            if side {
                rows.push(DisplayRow::SideLine {
                    left: Some(idx),
                    right: Some(idx),
                });
            } else {
                rows.push(DisplayRow::Line { idx });
            }
        };

        for (hi, h) in self.doc.hunks.iter().enumerate() {
            // Gap before the hunk: collapsed fold or expanded context lines.
            self.emit_gap(&mut rows, cursor, h.line_range.start, &emit_context);
            sep_rows.push(rows.len());
            rows.push(DisplayRow::HunkSep { hunk: hi });
            if side {
                self.emit_side_lines(&mut rows, h.line_range.clone());
            } else {
                for idx in h.line_range.clone() {
                    rows.push(DisplayRow::Line { idx });
                }
            }
            cursor = h.line_range.end;
        }
        self.emit_gap(&mut rows, cursor, n, &emit_context);

        self.total_rows = rows.len();
        self.rows = rows;
        self.sep_rows = sep_rows;
    }

    fn emit_gap(
        &self,
        rows: &mut Vec<DisplayRow>,
        start: usize,
        end: usize,
        emit_context: &impl Fn(&mut Vec<DisplayRow>, usize),
    ) {
        if end <= start {
            return;
        }
        if self.expanded.contains(&start) {
            for idx in start..end {
                emit_context(rows, idx); // gap lines are always context
            }
        } else {
            rows.push(DisplayRow::Fold {
                start,
                hidden: end - start,
            });
        }
    }

    /// Emit side-by-side rows for a hunk's line range, pairing contiguous
    /// removed/added blocks by character similarity.
    fn emit_side_lines(&self, rows: &mut Vec<DisplayRow>, range: std::ops::Range<usize>) {
        let lines = &self.doc.lines;
        let mut i = range.start;
        while i < range.end {
            if !lines[i].is_change() {
                rows.push(DisplayRow::SideLine {
                    left: Some(i),
                    right: Some(i),
                });
                i += 1;
                continue;
            }
            // Collect a contiguous run of changes; within it removed precede
            // added (how `similar` orders a replaced region).
            let run_start = i;
            while i < range.end && lines[i].is_change() {
                i += 1;
            }
            let removed: Vec<usize> = (run_start..i)
                .filter(|&j| lines[j].old_lineno().is_some())
                .collect();
            let added: Vec<usize> = (run_start..i)
                .filter(|&j| lines[j].new_lineno().is_some())
                .collect();
            for (l, r) in pair_changes(&removed, &added, lines) {
                rows.push(DisplayRow::SideLine { left: l, right: r });
            }
        }
    }

    /// The maximal unchanged ranges between hunks (the foldable regions). Each
    /// range's start is the key used in `expanded`.
    fn gaps(&self) -> Vec<std::ops::Range<usize>> {
        let n = self.doc.lines.len();
        let mut gaps = Vec::new();
        let mut cursor = 0;
        for h in &self.doc.hunks {
            if h.line_range.start > cursor {
                gaps.push(cursor..h.line_range.start);
            }
            cursor = h.line_range.end;
        }
        if n > cursor {
            gaps.push(cursor..n);
        }
        gaps
    }

    /// The `doc.lines` index shown at a given display row, if it's a line row.
    fn doc_line_at(&self, row: usize) -> Option<usize> {
        match self.rows.get(row)? {
            DisplayRow::Line { idx } => Some(*idx),
            DisplayRow::SideLine { left, right } => (*left).or(*right),
            _ => None,
        }
    }

    /// Index of the hunk considered "current" given the scroll position: the
    /// last hunk whose separator is at or above the top, else the first.
    fn current_hunk(&self) -> usize {
        self.sep_rows
            .iter()
            .rposition(|&r| r <= self.scroll)
            .unwrap_or(0)
    }

    /// Separator row of the next/previous hunk relative to the current one.
    fn hunk_row(&self, dir: isize) -> Option<usize> {
        if self.sep_rows.is_empty() {
            return None;
        }
        let cur = self.current_hunk() as isize;
        let target = (cur + dir).clamp(0, self.sep_rows.len() as isize - 1) as usize;
        Some(self.sep_rows[target])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Size,
    Path,
    Status,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Size => "size",
            SortMode::Path => "path",
            SortMode::Status => "status",
        }
    }
    fn next(self) -> SortMode {
        match self {
            SortMode::Size => SortMode::Path,
            SortMode::Path => SortMode::Status,
            SortMode::Status => SortMode::Size,
        }
    }
}

/// Overview-screen state.
pub struct Overview {
    /// Index into `order` of the highlighted row.
    pub selected: usize,
    /// First visible row (scroll offset into `order`).
    pub offset: usize,
    pub sort: SortMode,
    /// Live filter text; empty means no filter.
    pub filter: String,
    /// True while `/` capture is active.
    pub filtering: bool,
    /// File indices (into `changeset.files`) in current display order.
    pub order: Vec<usize>,
    /// Multi-selected file indices (into `changeset.files`).
    pub marked: HashSet<usize>,
    /// Number of file rows visible in the list on the last render — used for
    /// page-sized motions and to report "N of M".
    pub viewport_rows: usize,
}

pub struct App {
    pub repo: Repo,
    pub changeset: Changeset,
    pub theme: Theme,
    pub grouping: Grouping,
    pub viewed: Viewed,
    pub screen: Screen,
    pub ov: Overview,
    pub diff: Option<DiffState>,
    pub syntax: crate::syntax::Syntax,
    /// Per-file analyzed symbols/usages for the related-files index, computed
    /// lazily on first diff open and cached for the session.
    facts: Option<Vec<crate::syntax::FileFacts>>,
    /// Cursor into the current file's related list for `]r`/`[r`.
    related_cursor: usize,
    /// Mode forced on the command line (`--unified`/`--side-by-side`).
    pub forced_mode: Option<crate::cli::ForcedMode>,
    /// Per-session mode override once the user toggles with `s` (Phase 7);
    /// suppresses width-based auto-switching thereafter.
    pub mode_override: Option<ViewMode>,
    pub show_help: bool,
    /// Read-only commits-list overlay (placeholder for the v2 commits screen).
    pub show_commits: bool,
    /// "Explain these changes" query/overlay state.
    pub explain: crate::explain::Explain,
    /// "Commit reviewed files" message-prompt state (uncommitted view only).
    pub commit: crate::commit::Commit,
    /// Model for the `e` command, from `.rudiff.toml` (`None` => claude default).
    explain_model: Option<crate::explain::ExplainModel>,
    /// The active group config, retained so the rollup can be rebuilt when the
    /// file set changes (toggling untracked files in the uncommitted view).
    config: Option<Config>,
    /// In the uncommitted-changes view, whether untracked files are shown.
    /// Toggled with `t`; ignored outside that view.
    show_untracked: bool,
    /// True while `/` search capture is active in the diff view.
    pub diff_searching: bool,
    /// Pending first key of a two-key sequence (`g`, `z`, `]`, `[`).
    pending: Option<char>,
    /// Transient status message shown in the footer (e.g. after `o` on the
    /// overview), with the frames remaining before it clears.
    pub flash: Option<String>,
    /// Terminal width from the most recent render; drives layout auto-switch.
    pub width: u16,
    should_quit: bool,
}

impl App {
    pub fn new(repo: Repo, changeset: Changeset, cli: &Cli, config: Option<Config>) -> App {
        let grouping = group::build(&changeset.files, config.as_ref());
        let explain_model = config.as_ref().and_then(|c| c.explain_model());
        // Matches the initial build in `main` (untracked shown in the
        // uncommitted view); only consulted while `changeset.is_working`.
        let show_untracked = true;
        let viewed = Viewed::load(repo.git_dir());
        let order: Vec<usize> = (0..changeset.files.len()).collect();
        let theme = Theme::detect();
        let syntax = crate::syntax::Syntax::new(&theme);
        let mut app = App {
            repo,
            changeset,
            theme,
            grouping,
            viewed,
            screen: Screen::Overview,
            syntax,
            facts: None,
            related_cursor: 0,
            ov: Overview {
                selected: 0,
                offset: 0,
                sort: SortMode::Size,
                filter: String::new(),
                filtering: false,
                order,
                marked: HashSet::new(),
                viewport_rows: 1,
            },
            diff: None,
            forced_mode: cli.forced_mode(),
            mode_override: None,
            show_help: false,
            show_commits: false,
            explain: crate::explain::Explain::Idle,
            commit: crate::commit::Commit::Idle,
            explain_model,
            config,
            show_untracked,
            diff_searching: false,
            pending: None,
            flash: None,
            width: 0,
            should_quit: false,
        };
        app.recompute_order();
        app
    }

    /// Layout the width heuristic would pick (ignoring any user override).
    pub fn auto_mode(&self) -> ViewMode {
        if self.width >= SIDE_BY_SIDE_MIN_WIDTH {
            ViewMode::SideBySide
        } else {
            ViewMode::Unified
        }
    }

    /// True when the layout is pinned by a CLI flag or a session toggle, so
    /// width-based auto-switching should not override it.
    pub fn mode_is_pinned(&self) -> bool {
        self.forced_mode.is_some() || self.mode_override.is_some()
    }

    pub fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        while !self.should_quit {
            // Pick up output from an in-flight `claude` query before drawing.
            self.explain.poll();
            terminal.draw(|f| ui::draw(self, f))?;
            // Poll faster while a query runs so the spinner animates and the
            // result appears promptly.
            let timeout = if self.explain.is_running() {
                Duration::from_millis(80)
            } else {
                Duration::from_millis(150)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) => self.on_key(key),
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
        // Kill any in-flight query and persist viewed status on a clean exit.
        self.explain.cancel();
        self.viewed.save();
        Ok(())
    }

    /// Drive the app with a comma-separated key script (for `--keys` testing).
    /// Tokens: a single char is that key; `CR`/`ESC`/`SP`/`BS`/`TAB`; `C-x`
    /// for ctrl-modified keys.
    pub fn feed_keys(&mut self, spec: &str) {
        for tok in spec.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            let key = match tok {
                "CR" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                "ESC" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                "SP" => KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                "BS" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                "TAB" => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
                t if t.len() == 3 && t.starts_with("C-") => KeyEvent::new(
                    KeyCode::Char(t.chars().nth(2).unwrap()),
                    KeyModifiers::CONTROL,
                ),
                t => KeyEvent::new(KeyCode::Char(t.chars().next().unwrap()), KeyModifiers::NONE),
            };
            self.on_key(key);
        }
    }

    // ---- Event handling ----

    fn on_key(&mut self, key: KeyEvent) {
        // With the Kitty protocol we also receive key-release events; ignore
        // everything but presses (and repeats) so each press acts once.
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.flash = None;

        // Overlays swallow all keys: any key dismisses them.
        if self.show_help {
            self.show_help = false;
            return;
        }
        if self.show_commits {
            self.show_commits = false;
            return;
        }
        if self.explain.is_active() {
            self.on_explain_key(key);
            return;
        }
        if self.commit.is_active() {
            self.on_commit_key(key);
            return;
        }

        // Filter input mode captures typing.
        if self.ov.filtering {
            self.on_filter_key(key);
            return;
        }
        if self.diff_searching {
            self.on_search_key(key);
            return;
        }

        // Two-key sequences.
        if let Some(prefix) = self.pending.take()
            && self.on_sequence(prefix, key)
        {
            return;
        }
        // Fall through: treat `key` as a fresh keypress.

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => self.should_quit = true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Char('?'), _) => self.show_help = true,
            (KeyCode::Char('e'), _) => self.explain_current(),
            (KeyCode::Esc, _) if self.screen == Screen::Diff => self.back_to_overview(),
            _ => match self.screen {
                Screen::Overview => self.on_overview_key(key),
                Screen::Diff => self.on_diff_key(key),
            },
        }
    }

    /// Keys while the explain overlay is up. While the query runs, only `esc`
    /// (cancel) is honored; once a result is shown, `j`/`k` scroll and any of
    /// `esc`/`q`/`enter`/`e` dismiss it.
    fn on_explain_key(&mut self, key: KeyEvent) {
        // Guidance input: type to refine the prompt, enter to ask, esc to back out.
        if self.explain.is_prompting() {
            match key.code {
                KeyCode::Esc => self.explain.cancel(),
                KeyCode::Enter => self.explain.submit(),
                KeyCode::Backspace => self.explain.input_backspace(),
                KeyCode::Char(c) => self.explain.input_push(c),
                _ => {}
            }
            return;
        }
        // Save-filename editor: type to edit, enter to write, esc to back out.
        if self.explain.is_saving() {
            match key.code {
                KeyCode::Esc => self.explain.cancel_save(),
                KeyCode::Enter => {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    self.explain.confirm_save(&cwd);
                }
                KeyCode::Backspace => self.explain.save_input_backspace(),
                KeyCode::Char(c) => self.explain.save_input_push(c),
                _ => {}
            }
            return;
        }
        if self.explain.is_running() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                self.explain.cancel();
                self.flash = Some("explanation canceled".to_string());
            }
            return;
        }
        // Result shown.
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.explain.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.explain.scroll_by(-1),
            KeyCode::Char('s') => self.explain.start_save(),
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('e') => {
                self.explain.dismiss();
            }
            _ => {}
        }
    }

    /// Keys while the commit-message prompt is up: type the message, enter to
    /// commit, esc to cancel.
    fn on_commit_key(&mut self, key: KeyEvent) {
        if !self.commit.is_prompting() {
            return;
        }
        match key.code {
            KeyCode::Esc => self.commit = crate::commit::Commit::Idle,
            KeyCode::Enter => self.do_commit(),
            KeyCode::Backspace => self.commit.input_backspace(),
            KeyCode::Char(c) => self.commit.input_push(c),
            _ => {}
        }
    }

    /// Open the commit-message prompt for the reviewed files. Only meaningful in
    /// the uncommitted view; needs at least one file marked viewed.
    fn start_commit(&mut self) {
        if !self.changeset.is_working {
            self.flash = Some("commit applies only to --uncommitted".to_string());
            return;
        }
        // Reviewed files, in the current display order, as working-tree paths.
        let files: Vec<std::path::PathBuf> = self
            .ov
            .order
            .iter()
            .map(|&fi| &self.changeset.files[fi])
            .filter(|fc| self.viewed.is_viewed(fc.content_hash))
            .map(|fc| fc.path.clone())
            .collect();
        if files.is_empty() {
            self.flash = Some("no reviewed files to commit (mark with v)".to_string());
            return;
        }
        self.commit = crate::commit::Commit::prompt(files);
    }

    /// Stage and commit the reviewed files with the typed message. On success,
    /// refresh the working changeset (the committed files drop out) and report
    /// it; on failure, keep the prompt open with the error so the user can fix
    /// it and retry.
    fn do_commit(&mut self) {
        let (files, message) = match &self.commit {
            crate::commit::Commit::Prompting(p) => (p.files.clone(), p.input.trim().to_string()),
            _ => return,
        };
        if message.is_empty() {
            self.commit.set_notice("enter a commit message".to_string());
            return;
        }
        match crate::commit::run_commit(self.repo.root(), &files, &message) {
            Ok(summary) => {
                self.commit = crate::commit::Commit::Idle;
                let n = files.len();
                if let Err(e) = self.rebuild_working() {
                    self.flash = Some(format!("committed {summary}, but refresh failed: {e}"));
                } else {
                    self.flash = Some(format!(
                        "committed {} — {summary}",
                        crate::util::plural(n, "file")
                    ));
                }
            }
            Err(e) => self.commit.set_notice(e),
        }
    }

    /// Ask `claude -p` to explain the current file (in the diff view) or the
    /// whole changeset (on the overview).
    fn explain_current(&mut self) {
        let (instruction, diff_text, target) = match self.screen {
            Screen::Diff => {
                let Some(d) = &self.diff else { return };
                let fc = &self.changeset.files[d.file_index];
                let text = crate::explain::render_unified(&fc.path, &d.doc);
                let instruction = format!(
                    "You are helping review a pull request. Concisely explain this change to \
                     `{}` for a reviewer: what it does and why it likely matters. Use a few short \
                     bullet points; don't restate the diff line by line.",
                    fc.path.display()
                );
                (instruction, text, fc.path.display().to_string())
            }
            Screen::Overview => {
                let text = self.changeset_diff_text();
                let instruction = format!(
                    "You are helping review a pull request ({} → {}). Concisely explain what this \
                     branch changes overall and why, for a reviewer. Lead with a one-sentence \
                     summary, then a few short bullets grouped by area.",
                    self.changeset.head_name, self.changeset.base_name
                );
                let target = format!(
                    "{} ({} files)",
                    self.changeset.head_name,
                    self.changeset.files.len()
                );
                (instruction, text, target)
            }
        };

        if diff_text.trim().is_empty() {
            self.flash = Some("nothing to explain here".to_string());
            return;
        }
        // Open the guidance popup; the query fires on submit.
        self.explain =
            crate::explain::Explain::prompt(instruction, diff_text, target, self.explain_model);
    }

    /// Concatenated unified diff of the whole changeset, capped to keep the
    /// prompt small. Binary/submodule files are noted but not expanded.
    fn changeset_diff_text(&self) -> String {
        let mut out = String::new();
        for fc in &self.changeset.files {
            if out.len() >= crate::explain::MAX_DIFF_BYTES {
                out.push_str("\n… (remaining files omitted)\n");
                break;
            }
            if fc.is_binary() {
                out.push_str(&format!("# {} (binary, not shown)\n", fc.path.display()));
                continue;
            }
            let fd = self
                .repo
                .load_file_diff(fc, crate::git::diff::CONTEXT_RADIUS, false);
            out.push_str(&crate::explain::render_unified(&fc.path, &fd));
            out.push('\n');
        }
        out
    }

    /// Handle the second key of a sequence. Returns true if consumed.
    fn on_sequence(&mut self, prefix: char, key: KeyEvent) -> bool {
        match (prefix, key.code, self.screen) {
            ('g', KeyCode::Char('g'), Screen::Overview) => {
                self.cursor_to(0);
                true
            }
            ('g', KeyCode::Char('g'), Screen::Diff) => {
                self.diff_scroll_to(0);
                true
            }
            (']', KeyCode::Char('h'), Screen::Diff) => {
                self.diff_jump_hunk(1);
                true
            }
            ('[', KeyCode::Char('h'), Screen::Diff) => {
                self.diff_jump_hunk(-1);
                true
            }
            (']', KeyCode::Char('f'), Screen::Diff) => {
                self.diff_change_file(1);
                true
            }
            ('[', KeyCode::Char('f'), Screen::Diff) => {
                self.diff_change_file(-1);
                true
            }
            (']', KeyCode::Char('r'), Screen::Diff) => {
                self.diff_jump_related(1);
                true
            }
            ('[', KeyCode::Char('r'), Screen::Diff) => {
                self.diff_jump_related(-1);
                true
            }
            ('z', KeyCode::Char('R'), Screen::Diff) => {
                self.fold_all(true);
                true
            }
            ('z', KeyCode::Char('M'), Screen::Diff) => {
                self.fold_all(false);
                true
            }
            ('z', _, Screen::Diff) => {
                // `z` already expanded a fold when pressed; a non-R/M key just
                // falls through to normal handling.
                false
            }
            _ => false,
        }
    }

    fn on_overview_key(&mut self, key: KeyEvent) {
        let page = self.ov.viewport_rows.max(1) / 2;
        match (key.code, key.modifiers) {
            (KeyCode::Char('j') | KeyCode::Down, _) => self.cursor_by(1),
            (KeyCode::Char('k') | KeyCode::Up, _) => self.cursor_by(-1),
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.cursor_by(page as isize),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.cursor_by(-(page as isize)),
            (KeyCode::Char('g'), _) => self.pending = Some('g'),
            (KeyCode::Char('G'), _) => self.cursor_to(self.ov.order.len().saturating_sub(1)),
            (KeyCode::Char('s'), _) => self.cycle_sort(),
            (KeyCode::Char('/'), _) => {
                self.ov.filtering = true;
            }
            (KeyCode::Char('v'), _) => self.toggle_viewed_selection(),
            (KeyCode::Char('t'), _) => self.toggle_untracked(),
            (KeyCode::Char('C'), _) => self.start_commit(),
            (KeyCode::Char(' '), _) => self.toggle_mark(),
            (KeyCode::Char('c'), _) => self.show_commits = true,
            (KeyCode::Char('o'), _) => {
                self.flash = Some("already on overview".to_string());
            }
            (KeyCode::Enter, _) => self.open_diff(),
            _ => {}
        }
    }

    fn on_diff_key(&mut self, key: KeyEvent) {
        let page = self
            .diff
            .as_ref()
            .map(|d| d.viewport_rows.max(1) / 2)
            .unwrap_or(1);
        match (key.code, key.modifiers) {
            (KeyCode::Char('j') | KeyCode::Down, _) => self.diff_scroll_by(1),
            (KeyCode::Char('k') | KeyCode::Up, _) => self.diff_scroll_by(-1),
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => self.diff_scroll_by(page as isize),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.diff_scroll_by(-(page as isize)),
            (KeyCode::Char('g'), _) => self.pending = Some('g'),
            (KeyCode::Char('G'), _) => {
                let max = self.diff.as_ref().map(|d| d.total_rows).unwrap_or(0);
                self.diff_scroll_to(max);
            }
            (KeyCode::Char(']'), _) => self.pending = Some(']'),
            (KeyCode::Char('['), _) => self.pending = Some('['),
            (KeyCode::Char('z'), _) => {
                // Expand immediately for responsiveness; arm `zR`/`zM` in case
                // a R/M follows.
                self.fold_expand_nearest();
                self.pending = Some('z');
            }
            (KeyCode::Char('Z'), _) => self.fold_collapse_nearest(),
            (KeyCode::Char('o'), _) => self.back_to_overview(),
            (KeyCode::Char('v'), _) => self.diff_mark_viewed_advance(),
            (KeyCode::Char('s'), _) => self.toggle_mode(),
            (KeyCode::Char('w'), _) => self.cycle_whitespace(),
            (KeyCode::Char('/'), _) => {
                self.diff_searching = true;
                if let Some(d) = &mut self.diff {
                    d.search.clear();
                    d.matches.clear();
                }
            }
            (KeyCode::Char('n'), _) => self.search_jump(1),
            (KeyCode::Char('N'), _) => self.search_jump(-1),
            _ => {}
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.diff_searching = false;
                if let Some(d) = &mut self.diff {
                    d.search.clear();
                    d.matches.clear();
                }
            }
            KeyCode::Enter => self.diff_searching = false,
            KeyCode::Backspace => {
                if let Some(d) = &mut self.diff {
                    d.search.pop();
                }
                self.recompute_matches(true);
            }
            KeyCode::Char(c) => {
                if let Some(d) = &mut self.diff {
                    d.search.push(c);
                }
                self.recompute_matches(true);
            }
            _ => {}
        }
    }

    /// Recompute search matches over the current rows. When `jump`, move to the
    /// first match at or after the current scroll.
    fn recompute_matches(&mut self, jump: bool) {
        let Some(d) = &mut self.diff else { return };
        d.matches.clear();
        if d.search.is_empty() {
            return;
        }
        let needle = d.search.to_lowercase();
        for (row, dr) in d.rows.iter().enumerate() {
            let idxs: [Option<usize>; 2] = match dr {
                DisplayRow::Line { idx } => [Some(*idx), None],
                DisplayRow::SideLine { left, right } => [*left, *right],
                _ => [None, None],
            };
            if idxs
                .iter()
                .flatten()
                .any(|&i| d.doc.lines[i].content().to_lowercase().contains(&needle))
            {
                d.matches.push(row);
            }
        }
        if jump {
            let scroll = d.scroll;
            d.match_idx = d.matches.iter().position(|&r| r >= scroll).unwrap_or(0);
            if let Some(&row) = d.matches.get(d.match_idx) {
                d.scroll = row;
            }
        }
    }

    fn search_jump(&mut self, dir: isize) {
        let Some(d) = &mut self.diff else { return };
        if d.matches.is_empty() {
            self.flash = Some(if d.search.is_empty() {
                "no active search".to_string()
            } else {
                format!("no matches for \"{}\"", d.search)
            });
            return;
        }
        let len = d.matches.len() as isize;
        d.match_idx = ((d.match_idx as isize + dir).rem_euclid(len)) as usize;
        d.scroll = d.matches[d.match_idx];
        let (i, n) = (d.match_idx + 1, d.matches.len());
        self.flash = Some(format!("match {i}/{n}"));
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.ov.filtering = false;
                self.ov.filter.clear();
                self.recompute_order();
            }
            KeyCode::Enter => {
                self.ov.filtering = false;
            }
            KeyCode::Backspace => {
                self.ov.filter.pop();
                self.recompute_order();
            }
            KeyCode::Char(c) => {
                self.ov.filter.push(c);
                self.recompute_order();
            }
            _ => {}
        }
    }

    // ---- Overview operations ----

    fn cursor_by(&mut self, delta: isize) {
        if self.ov.order.is_empty() {
            return;
        }
        let max = self.ov.order.len() as isize - 1;
        let next = (self.ov.selected as isize + delta).clamp(0, max) as usize;
        self.ov.selected = next;
    }

    fn cursor_to(&mut self, idx: usize) {
        if self.ov.order.is_empty() {
            return;
        }
        self.ov.selected = idx.min(self.ov.order.len() - 1);
    }

    fn cycle_sort(&mut self) {
        // Keep the currently-selected file selected across the re-sort.
        let current = self.selected_file_index();
        self.ov.sort = self.ov.sort.next();
        self.recompute_order();
        if let Some(fi) = current
            && let Some(pos) = self.ov.order.iter().position(|&i| i == fi)
        {
            self.ov.selected = pos;
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(fi) = self.selected_file_index()
            && !self.ov.marked.insert(fi)
        {
            self.ov.marked.remove(&fi);
        }
    }

    fn toggle_viewed_selection(&mut self) {
        // Apply to the multi-selected set if any, else the current file.
        let targets: Vec<usize> = if self.ov.marked.is_empty() {
            self.selected_file_index().into_iter().collect()
        } else {
            self.ov.marked.iter().copied().collect()
        };
        if targets.is_empty() {
            return;
        }
        // If any target is unviewed, mark all viewed; else unview all.
        let any_unviewed = targets
            .iter()
            .any(|&fi| !self.viewed.is_viewed(self.changeset.files[fi].content_hash));
        for fi in targets {
            let h = self.changeset.files[fi].content_hash;
            self.viewed.set_viewed(h, any_unviewed);
        }
        self.ov.marked.clear();
        self.viewed.save();
    }

    /// File index (into `changeset.files`) currently under the cursor.
    pub fn selected_file_index(&self) -> Option<usize> {
        self.ov.order.get(self.ov.selected).copied()
    }

    // ---- Diff view operations ----

    /// Open the selected file in the diff view.
    fn open_diff(&mut self) {
        if self.selected_file_index().is_none() {
            return;
        }
        let mode = self.initial_view_mode();
        self.related_cursor = 0;
        self.load_current_file(mode);
        self.screen = Screen::Diff;
    }

    /// (Re)load the diff for the file under `ov.selected` into `self.diff`,
    /// including syntax highlighting and per-hunk function context.
    fn load_current_file(&mut self, mode: ViewMode) {
        let Some(fi) = self.selected_file_index() else {
            return;
        };
        // Whitespace setting carries across files (Ignore re-diffs the file).
        let whitespace = self
            .diff
            .as_ref()
            .map(|d| d.whitespace)
            .unwrap_or(Whitespace::Show);
        let ignore_ws = whitespace == Whitespace::Ignore;
        let fc = &self.changeset.files[fi];
        let mut doc = self
            .repo
            .load_file_diff(fc, crate::git::diff::CONTEXT_RADIUS, ignore_ws);

        // Tree-sitter work: highlight both sides and label each hunk. All of
        // this is best-effort and silently no-ops for unknown languages.
        let lang = crate::syntax::Lang::from_path(&fc.path);
        let (mut old_hl, mut new_hl) = (None, None);
        if let Some(lang) = lang {
            let (old_text, new_text) = self.repo.file_texts(fc);
            old_hl = self.syntax.highlight(&old_text, lang);
            new_hl = self.syntax.highlight(&new_text, lang);
            let rows: Vec<usize> = doc.hunks.iter().map(|h| doc.hunk_context_row(h)).collect();
            let ctxs = self.syntax.contexts(&new_text, lang, &rows);
            for (h, ctx) in doc.hunks.iter_mut().zip(ctxs) {
                h.function_context = ctx;
            }
        }

        // Intra-line changed-char sets for paired lines (mode-independent).
        let (intra, ws_only) = compute_intra(&doc);

        let mut state = DiffState {
            scroll: 0,
            doc,
            file_index: fi,
            expanded: HashSet::new(),
            rows: Vec::new(),
            sep_rows: Vec::new(),
            viewport_rows: 1,
            total_rows: 0,
            mode,
            whitespace,
            old_hl,
            new_hl,
            related: Vec::new(),
            intra,
            ws_only,
            search: String::new(),
            matches: Vec::new(),
            match_idx: 0,
        };
        state.rebuild_rows();
        self.diff = Some(state);
        // Related-files panel (lazy index, cached for the session).
        let related = self.compute_related(fi);
        if let Some(d) = &mut self.diff {
            d.related = related;
        }
    }

    /// Build (once) the per-file symbol/usage facts for the whole changeset.
    fn ensure_facts(&mut self) {
        if self.facts.is_some() {
            return;
        }
        let mut all = Vec::with_capacity(self.changeset.files.len());
        for fc in &self.changeset.files {
            let facts = match crate::syntax::Lang::from_path(&fc.path) {
                Some(lang) => {
                    let (old_text, new_text) = self.repo.file_texts(fc);
                    // Prefer the head version; fall back to old (e.g. deleted).
                    let text = if !new_text.is_empty() {
                        new_text
                    } else {
                        old_text
                    };
                    self.syntax.analyze(&text, lang)
                }
                None => crate::syntax::FileFacts::default(),
            };
            all.push(facts);
        }
        self.facts = Some(all);
    }

    /// Compute the related-files list for file `a`: other changeset files that
    /// reference symbols defined in `a`, ranked by match count. The search
    /// spans the *entire* changeset (cross-language is the valuable case).
    fn compute_related(&mut self, a: usize) -> Vec<RelatedEntry> {
        self.ensure_facts();
        let facts = self.facts.as_ref().unwrap();
        let mine = &facts[a];
        if mine.defines.is_empty() {
            return Vec::new();
        }
        let mut entries: Vec<RelatedEntry> = Vec::new();
        for (b, fb) in facts.iter().enumerate() {
            if b == a {
                continue;
            }
            let matched: Vec<&String> = mine
                .defines
                .iter()
                .filter(|s| fb.uses.contains(*s))
                .collect();
            if matched.is_empty() {
                continue;
            }
            // Verb priority: a call site is the most specific signal, then an
            // import, then a bare reference.
            let (verb, sample) = if let Some(s) = matched.iter().find(|s| fb.calls.contains(**s)) {
                (crate::syntax::Verb::Calls, (*s).clone())
            } else if let Some(s) = matched.iter().find(|s| fb.imports.contains(**s)) {
                (crate::syntax::Verb::Imports, (*s).clone())
            } else {
                (crate::syntax::Verb::References, matched[0].clone())
            };
            entries.push(RelatedEntry {
                file_index: b,
                verb,
                count: matched.len(),
                sample,
            });
        }
        entries.sort_by(|x, y| {
            y.count.cmp(&x.count).then_with(|| {
                self.changeset.files[x.file_index]
                    .path
                    .cmp(&self.changeset.files[y.file_index].path)
            })
        });
        entries.truncate(3);
        entries
    }

    /// The mode to use when first entering the diff view: an explicit override
    /// or CLI flag wins; otherwise Phase 7's width heuristic (Unified for now).
    fn initial_view_mode(&self) -> ViewMode {
        if let Some(m) = self.mode_override {
            return m;
        }
        match self.forced_mode {
            Some(crate::cli::ForcedMode::Unified) => ViewMode::Unified,
            Some(crate::cli::ForcedMode::SideBySide) => ViewMode::SideBySide,
            None => self.auto_mode(),
        }
    }

    /// Reconcile the open diff's layout with the current width when nothing is
    /// pinned (called from the renderer so it tracks live resizes). Rebuilds
    /// rows if the mode flips, keeping the current hunk anchored.
    pub fn reconcile_mode(&mut self) {
        if self.mode_is_pinned() {
            return;
        }
        let desired = self.auto_mode();
        if let Some(d) = &mut self.diff
            && d.mode != desired
        {
            let anchor = d.current_hunk();
            d.mode = desired;
            d.rebuild_rows();
            d.scroll = d.sep_rows.get(anchor).copied().unwrap_or(0);
        }
    }

    fn back_to_overview(&mut self) {
        // Overview state (selection/scroll) is preserved, so this restores it.
        self.screen = Screen::Overview;
    }

    fn diff_scroll_by(&mut self, delta: isize) {
        if let Some(d) = &mut self.diff {
            let max = d.total_rows.saturating_sub(1) as isize;
            d.scroll = (d.scroll as isize + delta).clamp(0, max.max(0)) as usize;
        }
    }

    fn diff_scroll_to(&mut self, row: usize) {
        if let Some(d) = &mut self.diff {
            d.scroll = row.min(d.total_rows.saturating_sub(1));
        }
    }

    fn diff_jump_hunk(&mut self, dir: isize) {
        if let Some(d) = &self.diff
            && let Some(row) = d.hunk_row(dir)
        {
            self.diff_scroll_to(row);
        }
    }

    /// Advance/retreat through files in the current display order, staying in
    /// the diff view. Keeps the overview selection in sync.
    fn diff_change_file(&mut self, dir: isize) {
        if self.ov.order.is_empty() {
            return;
        }
        let max = self.ov.order.len() as isize - 1;
        let next = (self.ov.selected as isize + dir).clamp(0, max) as usize;
        if next == self.ov.selected {
            return;
        }
        self.ov.selected = next;
        self.related_cursor = 0;
        let mode = self
            .diff
            .as_ref()
            .map(|d| d.mode)
            .unwrap_or(ViewMode::Unified);
        self.load_current_file(mode);
    }

    /// Jump to the next/previous file in the current file's related list.
    fn diff_jump_related(&mut self, dir: isize) {
        let rel = match &self.diff {
            Some(d) => d.related.clone(),
            None => return,
        };
        if rel.is_empty() {
            self.flash = Some("no related files".to_string());
            return;
        }
        let len = rel.len();
        let cursor = if dir > 0 {
            self.related_cursor % len
        } else {
            (self.related_cursor + len - 1) % len
        };
        let target = rel[cursor].file_index;
        // Advance the cursor for the next press (keep it for related navigation).
        self.related_cursor = (cursor + 1) % len;

        let Some(pos) = self.ov.order.iter().position(|&i| i == target) else {
            self.flash = Some("related file is filtered out".to_string());
            return;
        };
        self.ov.selected = pos;
        let mode = self
            .diff
            .as_ref()
            .map(|d| d.mode)
            .unwrap_or(ViewMode::Unified);
        let keep = self.related_cursor;
        self.load_current_file(mode);
        self.related_cursor = keep; // load_current_file doesn't touch it, but be explicit
    }

    fn diff_mark_viewed_advance(&mut self) {
        if let Some(fi) = self.selected_file_index() {
            let h = self.changeset.files[fi].content_hash;
            self.viewed.set_viewed(h, true);
            self.viewed.save();
        }
        // Advance to the next file; if already last, drop back to the overview.
        if self.ov.selected + 1 < self.ov.order.len() {
            self.diff_change_file(1);
        } else {
            self.back_to_overview();
        }
    }

    fn toggle_mode(&mut self) {
        if let Some(d) = &mut self.diff {
            let anchor = d.current_hunk();
            d.mode = match d.mode {
                ViewMode::Unified => ViewMode::SideBySide,
                ViewMode::SideBySide => ViewMode::Unified,
            };
            self.mode_override = Some(d.mode);
            d.rebuild_rows();
            // Keep the same hunk in view across the layout change.
            d.scroll = d.sep_rows.get(anchor).copied().unwrap_or(0);
        }
    }

    /// Expand the fold nearest the current scroll position (`z`).
    fn fold_expand_nearest(&mut self) {
        if let Some(d) = &mut self.diff {
            let nearest = d
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| match r {
                    DisplayRow::Fold { start, .. } => Some((i, *start)),
                    _ => None,
                })
                .min_by_key(|(i, _)| (*i as isize - d.scroll as isize).unsigned_abs());
            if let Some((_, start)) = nearest {
                d.expanded.insert(start);
                d.rebuild_rows();
            }
        }
    }

    /// Collapse the fold region containing the current line (`Z`).
    fn fold_collapse_nearest(&mut self) {
        if let Some(d) = &mut self.diff {
            // Find a doc line at/near the top of the viewport.
            let line_idx = (d.scroll..d.rows.len())
                .find_map(|r| d.doc_line_at(r))
                .or_else(|| (0..d.scroll).rev().find_map(|r| d.doc_line_at(r)));
            if let Some(line_idx) = line_idx
                && let Some(gap) = d.gaps().into_iter().find(|g| g.contains(&line_idx))
            {
                d.expanded.remove(&gap.start);
                d.rebuild_rows();
            }
        }
    }

    /// Expand (`zR`) or collapse (`zM`) all folds in the file.
    fn fold_all(&mut self, expand: bool) {
        if let Some(d) = &mut self.diff {
            if expand {
                d.expanded = d.gaps().into_iter().map(|g| g.start).collect();
            } else {
                d.expanded.clear();
            }
            d.rebuild_rows();
        }
    }

    fn cycle_whitespace(&mut self) {
        let Some(d) = &mut self.diff else { return };
        d.whitespace = d.whitespace.next();
        let label = d.whitespace.label();
        let mode = d.mode;
        let anchor = d.current_hunk();
        // Reload so the diff reflects the new whitespace handling (Ignore
        // re-diffs the file); keep the current hunk in view.
        self.load_current_file(mode);
        if let Some(d) = &mut self.diff {
            d.scroll = d.sep_rows.get(anchor).copied().unwrap_or(0);
        }
        self.flash = Some(format!("whitespace: {label}"));
    }

    /// Recompute the display order from the current sort + filter, clamping the
    /// cursor to stay in bounds.
    /// Toggle whether untracked files appear in the uncommitted-changes view.
    /// Rebuilds the changeset (and the group rollup) so the stats, groups, and
    /// file list all stay consistent. No-op outside that view.
    fn toggle_untracked(&mut self) {
        if !self.changeset.is_working {
            self.flash = Some("untracked toggle applies only to --uncommitted".to_string());
            return;
        }
        self.show_untracked = !self.show_untracked;
        match self.rebuild_working() {
            Ok(()) => {
                let n = self.changeset.files.len();
                self.flash = Some(if self.show_untracked {
                    format!("showing untracked files ({n} total)")
                } else {
                    format!("hiding untracked files ({n} total)")
                });
            }
            Err(e) => {
                // Keep state consistent if the refresh failed.
                self.show_untracked = !self.show_untracked;
                self.flash = Some(format!("could not refresh: {e}"));
            }
        }
    }

    /// Rebuild the working-tree changeset (respecting the untracked toggle) and
    /// reset the derived view state. Used after toggling untracked files and
    /// after committing. Errors are returned for the caller to surface.
    fn rebuild_working(&mut self) -> anyhow::Result<()> {
        let cs = self.repo.build_working_changeset(self.show_untracked)?;
        self.changeset = cs;
        self.grouping = group::build(&self.changeset.files, self.config.as_ref());
        // File indices change meaning when the set changes; drop the transient
        // multi-selection rather than mis-targeting it.
        self.ov.marked.clear();
        self.ov.offset = 0;
        self.recompute_order();
        Ok(())
    }

    fn recompute_order(&mut self) {
        let filter = self.ov.filter.to_lowercase();
        let files = &self.changeset.files;
        let mut order: Vec<usize> = (0..files.len())
            .filter(|&i| {
                filter.is_empty()
                    || files[i]
                        .path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&filter)
            })
            .collect();

        match self.ov.sort {
            SortMode::Size => order.sort_by(|&a, &b| {
                let ca = files[a].additions + files[a].deletions;
                let cb = files[b].additions + files[b].deletions;
                cb.cmp(&ca).then_with(|| files[a].path.cmp(&files[b].path))
            }),
            SortMode::Path => order.sort_by(|&a, &b| files[a].path.cmp(&files[b].path)),
            SortMode::Status => order.sort_by(|&a, &b| {
                status_rank(files[a].status)
                    .cmp(&status_rank(files[b].status))
                    .then_with(|| files[a].path.cmp(&files[b].path))
            }),
        }

        self.ov.order = order;
        if self.ov.selected >= self.ov.order.len() {
            self.ov.selected = self.ov.order.len().saturating_sub(1);
        }
    }

    /// Count of files whose diff is marked viewed.
    pub fn viewed_count(&self) -> usize {
        self.changeset
            .files
            .iter()
            .filter(|f| self.viewed.is_viewed(f.content_hash))
            .count()
    }

    pub fn is_viewed(&self, file_index: usize) -> bool {
        self.viewed
            .is_viewed(self.changeset.files[file_index].content_hash)
    }

    /// Whether the rollup is driven by `.rudiff.toml` groups (vs directories).
    pub fn using_config_groups(&self) -> bool {
        self.grouping.from_config
    }

    /// Whether any file belongs to more than one group (drives the footnote).
    pub fn multi_group_overlap(&self) -> bool {
        self.grouping.multi_group
    }
}

/// Character-similarity threshold above which a removed line is paired with an
/// added line for side-by-side alignment (and intra-line highlighting).
pub const SIMILARITY_THRESHOLD: f32 = 0.5;

/// Beyond this many lines in a single change block we skip similarity pairing
/// (which is O(R×A)) and just stack removed-then-added.
const MAX_PAIR_BLOCK: usize = 400;

/// Greedily pair removed lines with the most similar unused added line whose
/// similarity exceeds the threshold. Returns `(left, right)` cells in render
/// order: paired/unpaired removed first (in order), then unpaired added.
pub fn pair_changes(
    removed: &[usize],
    added: &[usize],
    lines: &[crate::git::model::DiffLine],
) -> Vec<(Option<usize>, Option<usize>)> {
    if removed.is_empty() {
        return added.iter().map(|&a| (None, Some(a))).collect();
    }
    if added.is_empty() {
        return removed.iter().map(|&r| (Some(r), None)).collect();
    }
    if removed.len() + added.len() > MAX_PAIR_BLOCK {
        let mut out: Vec<_> = removed.iter().map(|&r| (Some(r), None)).collect();
        out.extend(added.iter().map(|&a| (None, Some(a))));
        return out;
    }

    let mut used = vec![false; added.len()];
    let mut pair_for: Vec<Option<usize>> = vec![None; removed.len()];
    for (ri, &r) in removed.iter().enumerate() {
        let mut best: Option<usize> = None;
        let mut best_score = SIMILARITY_THRESHOLD;
        for (ai, &a) in added.iter().enumerate() {
            if used[ai] {
                continue;
            }
            let score = similarity(lines[r].content(), lines[a].content());
            if score > best_score {
                best_score = score;
                best = Some(ai);
            }
        }
        if let Some(ai) = best {
            used[ai] = true;
            pair_for[ri] = Some(ai);
        }
    }

    let mut out = Vec::with_capacity(removed.len() + added.len());
    for (ri, &r) in removed.iter().enumerate() {
        match pair_for[ri] {
            Some(ai) => out.push((Some(r), Some(added[ai]))),
            None => out.push((Some(r), None)),
        }
    }
    for (ai, &a) in added.iter().enumerate() {
        if !used[ai] {
            out.push((None, Some(a)));
        }
    }
    out
}

/// Don't bother diffing characters in absurdly long lines.
const MAX_INTRA_LINE: usize = 2000;

type IntraMap = std::collections::HashMap<usize, std::collections::HashSet<usize>>;

/// For every paired removed/added line, compute (a) which character positions
/// changed (intra-line highlighting) and (b) the set of line indices whose
/// change is whitespace-only (used by the `dim` whitespace mode).
pub fn compute_intra(
    doc: &crate::git::model::FileDiff,
) -> (IntraMap, std::collections::HashSet<usize>) {
    use std::collections::HashSet;
    let lines = &doc.lines;
    let mut map: IntraMap = std::collections::HashMap::new();
    let mut ws_only: HashSet<usize> = HashSet::new();
    for h in &doc.hunks {
        let mut i = h.line_range.start;
        while i < h.line_range.end {
            if !lines[i].is_change() {
                i += 1;
                continue;
            }
            let run_start = i;
            while i < h.line_range.end && lines[i].is_change() {
                i += 1;
            }
            let removed: Vec<usize> = (run_start..i)
                .filter(|&j| lines[j].old_lineno().is_some())
                .collect();
            let added: Vec<usize> = (run_start..i)
                .filter(|&j| lines[j].new_lineno().is_some())
                .collect();
            for (l, r) in pair_changes(&removed, &added, lines) {
                if let (Some(l), Some(r)) = (l, r) {
                    let (lc, rc) = (lines[l].content(), lines[r].content());
                    let (lo, ro): (HashSet<usize>, HashSet<usize>) = char_changes(lc, rc);
                    map.insert(l, lo);
                    map.insert(r, ro);
                    if crate::git::diff::normalize_line(lc) == crate::git::diff::normalize_line(rc)
                    {
                        ws_only.insert(l);
                        ws_only.insert(r);
                    }
                }
            }
        }
    }
    (map, ws_only)
}

/// Character indices that changed between two paired lines: `(old_set,
/// new_set)`. Returns empty sets for over-long lines.
fn char_changes(
    old: &str,
    new: &str,
) -> (
    std::collections::HashSet<usize>,
    std::collections::HashSet<usize>,
) {
    use similar::ChangeTag;
    let mut o = std::collections::HashSet::new();
    let mut n = std::collections::HashSet::new();
    if old.len() > MAX_INTRA_LINE || new.len() > MAX_INTRA_LINE {
        return (o, n);
    }
    let diff = similar::TextDiff::from_chars(old, new);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => {
                o.insert(change.old_index().unwrap());
            }
            ChangeTag::Insert => {
                n.insert(change.new_index().unwrap());
            }
            ChangeTag::Equal => {}
        }
    }
    (o, n)
}

/// Character-level similarity in `0.0..=1.0` (Myers ratio via `similar`).
pub fn similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    similar::TextDiff::from_chars(a, b).ratio()
}

fn status_rank(s: crate::git::model::FileStatus) -> u8 {
    use crate::git::model::FileStatus::*;
    match s {
        Added => 0,
        Modified => 1,
        Renamed => 2,
        Copied => 3,
        Deleted => 4,
    }
}
