//! The diff view (unified mode in Phase 4; side-by-side added in Phase 7).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, DiffState, DisplayRow, ViewMode, Whitespace};
use crate::git::model::{DiffLine, FileChange, Special};
use crate::syntax::Seg;
use crate::theme::Theme;
use crate::ui::widgets::{footer, justified, pad_to, rule};
use crate::util::format_bytes;

const TABSTOP: usize = 4;

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    let w = area.width;
    let Some(d) = &app.diff else { return };
    let fc = &app.changeset.files[d.file_index];

    // Header (rule / file line / rule).
    let header = file_header(app, fc, w);

    // Related panel: header + entries + trailing blank, or hidden if empty.
    let related_h = match &d.related {
        r if r.is_empty() => 0,
        r => (r.len() + 2) as u16,
    };

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(related_h),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);

    f.render_widget(Paragraph::new(header), chunks[0]);
    if related_h > 0 {
        render_related(app, f, chunks[1]);
    }
    render_body(app, f, chunks[2]);
    render_footer(app, f, chunks[3]);
}

fn render_related(app: &App, f: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let d = app.diff.as_ref().unwrap();
    let w = area.width as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(d.related.len() + 2);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Related in this PR", Style::default().fg(theme.accent)),
    ]));
    // Column where the verb/sample starts, so paths align.
    let path_col = (w.saturating_sub(30)).clamp(20, 48);
    for entry in &d.related {
        let fc = &app.changeset.files[entry.file_index];
        let path =
            crate::util::truncate_left(&fc.path.to_string_lossy(), path_col.saturating_sub(6));
        let pad = path_col.saturating_sub(path.chars().count() + 4);
        let extra = if entry.count > 1 {
            format!(" (+{} more)", entry.count - 1)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(path, Style::default().fg(theme.renamed)),
            Span::raw(" ".repeat(pad)),
            Span::styled(format!("{} ", entry.verb.label()), theme.dim()),
            Span::styled(entry.sample.clone(), Style::default().fg(theme.secondary)),
            Span::styled(extra, theme.dim()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn file_header(app: &App, fc: &FileChange, w: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let viewed = app.is_viewed(app.diff.as_ref().unwrap().file_index);
    let dot = if viewed { "●" } else { "○" };
    let dot_style = if viewed {
        theme.dim()
    } else {
        Style::default().fg(theme.added)
    };

    let pos = app.ov.selected + 1;
    let total = app.ov.order.len();
    let path = fc
        .old_path
        .as_ref()
        .map(|o| format!("{} → {}", o.display(), fc.path.display()))
        .unwrap_or_else(|| fc.path.display().to_string());

    let left = vec![
        Span::raw("  "),
        Span::styled(dot.to_string(), dot_style),
        Span::raw(" "),
        Span::styled(
            fc.display_letter().to_string(),
            theme.status_style(fc.status),
        ),
        Span::raw(" "),
        Span::styled(path, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(format!("+{}", fc.additions), theme.added_style()),
        Span::raw(" / "),
        Span::styled(format!("-{}", fc.deletions), theme.removed_style()),
    ];
    let right = vec![
        Span::styled(format!("file {pos} of {total}"), theme.dim()),
        Span::styled("    o overview  ", theme.dim()),
    ];
    vec![rule(w, theme), justified(left, right, w), rule(w, theme)]
}

fn render_body(app: &mut App, f: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let w = area.width;
    let theme = app.theme;
    let d = app.diff.as_mut().unwrap();
    d.viewport_rows = area.height as usize;

    // Special-case placeholders that have no rows.
    let fc = &app.changeset.files[d.file_index];
    if let Special::Binary { old_size, new_size } = fc.special {
        let msg = format!(
            "Binary file changed ({} → {})",
            format_bytes(old_size),
            format_bytes(new_size)
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, theme.dim()))).alignment(Alignment::Center),
            area,
        );
        return;
    }
    if d.rows.is_empty() {
        let msg = if fc.old_path.is_some() {
            "No content changes (file renamed)."
        } else {
            "No changes."
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, theme.dim()))).alignment(Alignment::Center),
            area,
        );
        return;
    }

    // Clamp scroll so we never scroll into empty space below the diff.
    let rows = area.height as usize;
    let max_scroll = d.total_rows.saturating_sub(rows);
    if d.scroll > max_scroll {
        d.scroll = max_scroll;
    }

    let gutter = gutter_width(d);
    let total_hunks = d.doc.hunks.len();

    // A logical row can produce more than one terminal line (wrapped
    // side-by-side cells), so accumulate until the viewport is full.
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    let mut i = d.scroll;
    while i < d.rows.len() && lines.len() < rows {
        match &d.rows[i] {
            DisplayRow::HunkSep { hunk } => {
                lines.push(hunk_separator(
                    &d.doc.hunks[*hunk],
                    *hunk,
                    total_hunks,
                    w,
                    &theme,
                ));
            }
            DisplayRow::Fold { hidden, .. } => lines.push(fold_band(*hidden, w, &theme)),
            DisplayRow::Line { idx } => {
                lines.push(unified_line(d, *idx, gutter, w, &theme));
            }
            DisplayRow::SideLine { left, right } => {
                for l in side_row(d, *left, *right, gutter, w, &theme) {
                    lines.push(l);
                }
            }
        }
        i += 1;
    }
    lines.truncate(rows);
    f.render_widget(Paragraph::new(lines), area);
}

/// Width of the line-number gutter, sized to the largest line number.
fn gutter_width(d: &DiffState) -> usize {
    let max = d
        .doc
        .lines
        .last()
        .map(|l| l.new_lineno().or(l.old_lineno()).unwrap_or(0))
        .unwrap_or(0);
    max.to_string().len().clamp(3, 6)
}

fn hunk_separator(
    h: &crate::git::model::Hunk,
    idx: usize,
    total: usize,
    w: u16,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  ── hunk {} of {} ", idx + 1, total),
        theme.chrome_style(),
    )];
    if let Some(ctx) = &h.function_context {
        spans.push(Span::styled("──── ".to_string(), theme.chrome_style()));
        spans.push(Span::styled(
            ctx.clone(),
            Style::default().fg(theme.secondary),
        ));
        spans.push(Span::raw(" "));
    }
    let used: usize = spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let fill = (w as usize).saturating_sub(used);
    spans.push(Span::styled("─".repeat(fill), theme.chrome_style()));
    Line::from(spans)
}

fn fold_band(hidden: usize, w: u16, theme: &Theme) -> Line<'static> {
    let noun = if hidden == 1 { "line" } else { "lines" };
    let label = format!("  ── {hidden} unchanged {noun} (z to expand) ");
    let fill = (w as usize).saturating_sub(unicode_width::UnicodeWidthStr::width(label.as_str()));
    Line::from(vec![
        Span::styled(label, theme.dim()),
        Span::styled("─".repeat(fill), theme.chrome_style()),
    ])
}

fn unified_line(d: &DiffState, idx: usize, gutter: usize, w: u16, theme: &Theme) -> Line<'static> {
    let line = &d.doc.lines[idx];
    let (lineno, marker, mut marker_style, base_bg, mut strong_bg) = line_styling(line, theme);
    let num = lineno.map(|n| n.to_string()).unwrap_or_default();

    // In `dim` whitespace mode, de-emphasize whitespace-only changes: drop
    // syntax/intra coloring and render the line dim.
    let dim_ws = d.whitespace == Whitespace::Dim && d.ws_only.contains(&idx);
    let (segs, intra) = if dim_ws {
        marker_style = theme.dim();
        strong_bg = None;
        (vec![(line.content().to_string(), theme.dim())], None)
    } else {
        (line_segments(d, idx), d.intra.get(&idx))
    };

    // Content spans (tab-expanded, syntax-styled). The line-level background
    // fills the row; intra-line changed chars carry the stronger background,
    // so we only bake `strong_bg` here and leave the rest to inherit the base.
    let search = search_chars(line.content(), &d.search);
    let chars = expand_style(
        &segs,
        intra,
        search.as_ref(),
        None,
        strong_bg,
        theme.modified,
    );
    let content_spans = visual_lines(&chars, usize::MAX).pop().unwrap_or_default();

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(format!("{num:>gutter$}"), theme.dim()),
        Span::raw(" "),
        Span::styled(marker.to_string(), marker_style),
        Span::raw(" "),
    ];
    spans.extend(content_spans);
    let mut line = Line::from(pad_to(spans, w));
    if let Some(bg) = base_bg {
        line = line.style(Style::default().bg(bg));
    }
    line
}

/// Line-number side, marker char, marker style, base bg, and intra (strong) bg
/// for a diff line.
fn line_styling(
    line: &DiffLine,
    theme: &Theme,
) -> (
    Option<usize>,
    char,
    Style,
    Option<ratatui::style::Color>,
    Option<ratatui::style::Color>,
) {
    match line {
        DiffLine::Context { new_lineno, .. } => {
            (Some(*new_lineno), ' ', Style::default(), None, None)
        }
        DiffLine::Removed { old_lineno, .. } => (
            Some(*old_lineno),
            '-',
            Style::default()
                .fg(theme.removed)
                .add_modifier(Modifier::BOLD),
            Some(theme.bg_danger),
            Some(theme.bg_danger_strong),
        ),
        DiffLine::Added { new_lineno, .. } => (
            Some(*new_lineno),
            '+',
            Style::default()
                .fg(theme.added)
                .add_modifier(Modifier::BOLD),
            Some(theme.bg_success),
            Some(theme.bg_success_strong),
        ),
    }
}

/// Build the (possibly multi-line, wrapped) terminal rows for one side-by-side
/// visual row: old half `│` new half.
fn side_row(
    d: &DiffState,
    left: Option<usize>,
    right: Option<usize>,
    gutter: usize,
    w: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let sep_cols = 3u16; // " │ "
    let total = w.saturating_sub(sep_cols);
    let left_w = (total / 2) as usize;
    let right_w = (total - total / 2) as usize;

    let lhs = build_half(d, left, Side::Old, left_w, gutter, theme);
    let rhs = build_half(d, right, Side::New, right_w, gutter, theme);

    let n = lhs.len().max(rhs.len()).max(1);
    let mut out = Vec::with_capacity(n);
    for v in 0..n {
        let mut spans = lhs
            .get(v)
            .cloned()
            .unwrap_or_else(|| vec![Span::raw(" ".repeat(left_w))]);
        spans.push(Span::styled(" │ ".to_string(), theme.chrome_style()));
        let r = rhs
            .get(v)
            .cloned()
            .unwrap_or_else(|| vec![Span::raw(" ".repeat(right_w))]);
        spans.extend(r);
        out.push(Line::from(spans));
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Old,
    New,
}

/// Render one half-column of a side-by-side row, wrapping long content with a
/// `↪` continuation marker. Returns one span-vec per wrapped visual line, each
/// padded to `width` with the half background applied.
fn build_half(
    d: &DiffState,
    idx: Option<usize>,
    side: Side,
    width: usize,
    gutter: usize,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    let Some(idx) = idx else { return Vec::new() };
    let line = &d.doc.lines[idx];

    // Marker/numbers come from the side; the line itself knows its bg colors.
    let (lineno, marker, mut marker_style, base_bg, mut strong_bg) = match (side, line) {
        (Side::Old, DiffLine::Removed { .. }) | (Side::New, DiffLine::Added { .. }) => {
            line_styling(line, theme)
        }
        (Side::Old, DiffLine::Context { old_lineno, .. }) => {
            (Some(*old_lineno), ' ', Style::default(), None, None)
        }
        (Side::New, DiffLine::Context { new_lineno, .. }) => {
            (Some(*new_lineno), ' ', Style::default(), None, None)
        }
        // A removed line never appears on the new side, nor added on the old.
        _ => (None, ' ', Style::default(), None, None),
    };

    let dim_ws = d.whitespace == Whitespace::Dim && d.ws_only.contains(&idx);
    let (segs, intra) = if dim_ws {
        marker_style = theme.dim();
        strong_bg = None;
        (vec![(line.content().to_string(), theme.dim())], None)
    } else {
        (line_segments(d, idx), d.intra.get(&idx))
    };

    let prefix = gutter + 4; // " " + gutter + " " + marker + " "
    let content_w = width.saturating_sub(prefix).max(1);
    // Side-by-side has no row-level background, so bake both base and strong.
    let search = search_chars(line.content(), &d.search);
    let chars = expand_style(
        &segs,
        intra,
        search.as_ref(),
        base_bg,
        strong_bg,
        theme.modified,
    );
    let visuals = visual_lines(&chars, content_w);
    let num = lineno.map(|n| n.to_string()).unwrap_or_default();
    let pad_style = base_bg.map(|c| Style::default().bg(c)).unwrap_or_default();

    let mut out = Vec::with_capacity(visuals.len());
    for (k, content_spans) in visuals.into_iter().enumerate() {
        let (num_span, mark) = if k == 0 {
            (format!("{num:>gutter$}"), marker)
        } else {
            (" ".repeat(gutter), '↪')
        };
        let mut spans = vec![
            Span::styled(" ", pad_style),
            Span::styled(num_span, theme.dim().patch(pad_style)),
            Span::styled(" ", pad_style),
            Span::styled(
                mark.to_string(),
                if k == 0 {
                    marker_style.patch(pad_style)
                } else {
                    theme.dim().patch(pad_style)
                },
            ),
            Span::styled(" ", pad_style),
        ];
        spans.extend(content_spans);
        // Pad the half to its full width with the base background.
        let used: usize = spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let pad = (width).saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), pad_style));
        }
        out.push(spans);
    }
    out
}

/// Styled segments for one diff line: from the syntax cache (old side for
/// removed lines, new side otherwise), or a single plain segment as fallback.
fn line_segments(d: &DiffState, idx: usize) -> Vec<Seg> {
    let line = &d.doc.lines[idx];
    let (cache, lineno) = match line {
        DiffLine::Removed { old_lineno, .. } => (d.old_hl.as_ref(), *old_lineno),
        DiffLine::Added { new_lineno, .. } | DiffLine::Context { new_lineno, .. } => {
            (d.new_hl.as_ref(), *new_lineno)
        }
    };
    if let Some(cache) = cache
        && let Some(segs) = cache.get(lineno.wrapping_sub(1))
        && !segs.is_empty()
    {
        return segs.clone();
    }
    vec![(line.content().to_string(), Style::default())]
}

/// Expand a line's segments into per-character `(char, style)` with tabs turned
/// into spaces, baking in the background: `strong_bg` for intra-line changed
/// chars (by original char index), else `base_bg`.
fn expand_style(
    segs: &[Seg],
    intra: Option<&std::collections::HashSet<usize>>,
    search: Option<&std::collections::HashSet<usize>>,
    base_bg: Option<ratatui::style::Color>,
    strong_bg: Option<ratatui::style::Color>,
    search_bg: ratatui::style::Color,
) -> Vec<(char, Style)> {
    use unicode_width::UnicodeWidthChar;
    let mut out = Vec::new();
    let mut col = 0;
    let mut ci = 0; // original char index, for intra/search lookup
    for (text, style) in segs {
        for ch in text.chars() {
            // Priority: search hit > intra-line change > line background.
            let st = if search.is_some_and(|s| s.contains(&ci)) {
                style.bg(search_bg).fg(ratatui::style::Color::Black)
            } else {
                let bg = if intra.is_some_and(|s| s.contains(&ci)) {
                    strong_bg
                } else {
                    base_bg
                };
                match bg {
                    Some(c) => style.bg(c),
                    None => *style,
                }
            };
            if ch == '\t' {
                let n = TABSTOP - (col % TABSTOP);
                for _ in 0..n {
                    out.push((' ', st));
                }
                col += n;
            } else {
                out.push((ch, st));
                col += ch.width().unwrap_or(0);
            }
            ci += 1;
        }
    }
    out
}

/// Wrap `(char, style)` cells into visual lines of at most `width` display
/// columns (`usize::MAX` = no wrapping), coalescing same-styled runs into spans.
fn visual_lines(chars: &[(char, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    use unicode_width::UnicodeWidthChar;
    let mut lines = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut col = 0;
    for &(ch, st) in chars {
        let cw = ch.width().unwrap_or(0);
        if width != usize::MAX && col + cw > width && !cur.is_empty() {
            lines.push(coalesce(&cur));
            cur.clear();
            col = 0;
        }
        cur.push((ch, st));
        col += cw;
    }
    lines.push(coalesce(&cur));
    lines
}

/// Character indices in `content` covered by a (case-insensitive) match of
/// `query`. Empty query => no hits.
fn search_chars(content: &str, query: &str) -> Option<std::collections::HashSet<usize>> {
    if query.is_empty() {
        return None;
    }
    let hay = content.to_lowercase();
    let needle = query.to_lowercase();
    if !hay.contains(&needle) {
        return None;
    }
    // Work in char space to align with expand_style's char indexing.
    let chars: Vec<char> = content.chars().collect();
    let lower: Vec<char> = hay.chars().collect();
    let nlen = needle.chars().count();
    if nlen == 0 || lower.len() != chars.len() {
        // Lowercasing changed length (rare, e.g. ß); fall back to no highlight.
        return None;
    }
    let nchars: Vec<char> = needle.chars().collect();
    let mut hits = std::collections::HashSet::new();
    let mut i = 0;
    while i + nlen <= lower.len() {
        if lower[i..i + nlen] == nchars[..] {
            for j in i..i + nlen {
                hits.insert(j);
            }
            i += nlen;
        } else {
            i += 1;
        }
    }
    Some(hits)
}

/// Coalesce consecutive same-styled chars into `Span`s.
fn coalesce(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut cur_style: Option<Style> = None;
    for &(ch, st) in chars {
        if cur_style != Some(st) {
            if let Some(s) = cur_style.take() {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
            }
            cur_style = Some(st);
        }
        buf.push(ch);
    }
    if let Some(s) = cur_style {
        spans.push(Span::styled(buf, s));
    }
    spans
}

fn render_footer(app: &App, f: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    f.render_widget(Paragraph::new(rule(area.width, theme)), parts[0]);

    // Search input takes over the hint line while active.
    if app.diff_searching {
        let q = app.diff.as_ref().map(|d| d.search.as_str()).unwrap_or("");
        let n = app.diff.as_ref().map(|d| d.matches.len()).unwrap_or(0);
        let line = Line::from(vec![
            Span::styled(format!(" /{q}"), Style::default().fg(theme.accent)),
            Span::styled("_", Style::default().fg(theme.accent)),
            Span::styled(
                format!("   {n} matches  (enter to keep, esc to cancel)"),
                theme.dim(),
            ),
        ]);
        f.render_widget(Paragraph::new(line), parts[1]);
        return;
    }
    let mode_hint = match app.diff.as_ref().map(|d| d.mode) {
        Some(ViewMode::SideBySide) => ("s", "unified"),
        _ => ("s", "side-by-side"),
    };
    let hints = [
        ("]h", "hunk"),
        ("]f", "file"),
        ("v", "viewed"),
        mode_hint,
        ("/", "search"),
        ("o", "overview"),
        ("?", "help"),
        ("q", "quit"),
    ];
    f.render_widget(
        Paragraph::new(footer(&hints, app.flash.as_deref(), theme)),
        parts[1],
    );
}
