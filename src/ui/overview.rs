//! The overview screen: header, stats, reviewed bar, group rollup, file list.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::git::model::{FileChange, Special};
use crate::group::{self, Group};
use crate::ui::widgets::{footer, justified, pad_to, rule};
use crate::util::{bar, commas, plural, relative_age, truncate_left};

const REVIEWED_BAR_W: usize = 30;
const GROUP_BAR_W: usize = 24;
const SIZE_BAR_W: usize = 12;

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    let w = area.width;
    let theme = &app.theme;

    // Build the fixed top section first so we know its height.
    let mut top: Vec<Line> = vec![rule(w, theme)];
    top.push(justified(
        vec![
            Span::raw("  "),
            Span::styled(app.changeset.head_name.clone(), theme.header_style()),
            Span::styled("  →  ", theme.dim()),
            Span::styled(app.changeset.base_name.clone(), theme.header_style()),
        ],
        vec![Span::styled("press ? for help  ", theme.dim())],
        w,
    ));
    top.push(rule(w, theme));
    top.push(Line::from(""));
    top.push(stats_line(app));
    top.push(reviewed_line(app));
    top.push(Line::from(""));
    top.extend(group_section(app, w));
    top.push(Line::from(""));

    // Cap the top section so the file list and footer always have room.
    let max_top = area.height.saturating_sub(5);
    let top_h = (top.len() as u16).min(max_top);

    let chunks = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);

    f.render_widget(Paragraph::new(top), chunks[0]);
    render_files(app, f, chunks[1]);
    render_footer(app, f, chunks[2]);
}

fn stats_line(app: &App) -> Line<'static> {
    let cs = &app.changeset;
    let theme = &app.theme;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("{} changed", plural(cs.files.len(), "file")),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("+{}", commas(cs.total_additions())),
            theme.added_style(),
        ),
        Span::raw(" / "),
        Span::styled(
            format!("-{}", commas(cs.total_deletions())),
            theme.removed_style(),
        ),
        Span::styled(" lines", theme.dim()),
        Span::raw("   "),
        Span::styled(plural(cs.commits.len(), "commit"), theme.dim()),
        Span::raw("   "),
        Span::styled(plural(cs.author_count(), "author"), theme.dim()),
    ];
    if let Some(oldest) = cs.oldest_commit_secs() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(relative_age(oldest, now), theme.dim()));
    }
    Line::from(spans)
}

fn reviewed_line(app: &App) -> Line<'static> {
    let total = app.changeset.files.len();
    let viewed = app.viewed_count();
    let ratio = if total == 0 {
        0.0
    } else {
        viewed as f64 / total as f64
    };
    let pct = (ratio * 100.0).round() as u32;
    let theme = &app.theme;
    Line::from(vec![
        Span::raw("  "),
        Span::styled("Reviewed  ", theme.dim()),
        Span::styled(
            format!("{viewed}/{total}  "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(bar(ratio, REVIEWED_BAR_W, '━', '░'), theme.added_style()),
        Span::styled(format!("  {pct}%"), theme.dim()),
    ])
}

fn group_section(app: &App, w: u16) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut lines = Vec::new();
    let title = if app.using_config_groups() {
        "Changes by group"
    } else {
        "Changes by directory"
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            title.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    let max = group::max_churn(&app.grouping.groups).max(1);
    // Width budget for group name so bars align.
    let name_w = app
        .grouping
        .groups
        .iter()
        .map(|g| g.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, (w as usize).saturating_sub(40).max(8));

    for g in &app.grouping.groups {
        lines.push(group_row(g, max, name_w, theme));
    }
    if app.multi_group_overlap() {
        lines.push(Line::from(Span::styled(
            "  † files in multiple groups are counted once in totals".to_string(),
            theme.dim(),
        )));
    }
    lines
}

fn group_row(g: &Group, max: usize, name_w: usize, theme: &crate::theme::Theme) -> Line<'static> {
    let ratio = g.churn() as f64 / max as f64;
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:<width$}", trunc_name(&g.name, name_w), width = name_w),
            Style::default(),
        ),
        Span::raw("  "),
        Span::styled(format!("{:>3} files", g.file_count), theme.dim()),
        Span::raw("   "),
        Span::styled(format!("+{:<5}", commas(g.additions)), theme.added_style()),
        Span::styled(
            format!("-{:<5}", commas(g.deletions)),
            theme.removed_style(),
        ),
        Span::raw(" "),
        Span::styled(
            bar(ratio, GROUP_BAR_W, '█', '░'),
            Style::default().fg(theme.accent),
        ),
    ])
}

fn trunc_name(name: &str, w: usize) -> String {
    crate::util::truncate_right(name, w)
}

fn render_files(app: &mut App, f: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let w = area.width;

    // Split: one header line, then the scrollable list.
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    let list_area = parts[1];
    let rows = list_area.height as usize;
    app.ov.viewport_rows = rows;

    let total = app.ov.order.len();
    // Keep the cursor visible.
    if app.ov.selected < app.ov.offset {
        app.ov.offset = app.ov.selected;
    } else if rows > 0 && app.ov.selected >= app.ov.offset + rows {
        app.ov.offset = app.ov.selected + 1 - rows;
    }
    let shown = rows.min(total.saturating_sub(app.ov.offset));

    // Header line: "Files (X of Y)            sort: size ▾"
    let header = justified(
        vec![
            Span::raw("  "),
            Span::styled(
                format!("Files ({} of {})", shown, total),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            filter_hint(app),
        ],
        vec![Span::styled(
            format!("sort: {} ▾  ", app.ov.sort.label()),
            app.theme.dim(),
        )],
        w,
    );
    f.render_widget(Paragraph::new(header), parts[0]);

    // Empty states: nothing changed, or a filter that matches nothing.
    if total == 0 {
        let msg = if app.changeset.files.is_empty() {
            "No changes between these revisions."
        } else {
            "No files match the filter."
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {msg}"),
                app.theme.dim(),
            ))),
            list_area,
        );
        return;
    }

    // Largest churn across all files for size-bar scaling.
    let max_churn = app
        .changeset
        .files
        .iter()
        .map(|fc| fc.additions + fc.deletions)
        .max()
        .unwrap_or(0)
        .max(1);

    let mut lines: Vec<Line> = Vec::with_capacity(shown);
    for row in 0..shown {
        let order_idx = app.ov.offset + row;
        let fi = app.ov.order[order_idx];
        let selected = order_idx == app.ov.selected;
        lines.push(file_row(app, fi, selected, max_churn, w, &app.theme));
    }
    // If rows are hidden below the fold, replace the last visible row with a
    // "↓ N more" indicator so the user knows the list continues.
    if total > app.ov.offset + shown && !lines.is_empty() {
        let more = total - (app.ov.offset + shown) + 1;
        let label = format!("↓ {more} more");
        let pad = (w as usize).saturating_sub(label.chars().count() + 2);
        *lines.last_mut().unwrap() = Line::from(Span::styled(
            format!("{}{}", " ".repeat(pad), label),
            app.theme.dim(),
        ));
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

fn filter_hint(app: &App) -> Span<'static> {
    if app.ov.filtering {
        Span::styled(
            format!("   /{}_", app.ov.filter),
            Style::default().fg(app.theme.accent),
        )
    } else if !app.ov.filter.is_empty() {
        Span::styled(format!("   /{}", app.ov.filter), app.theme.dim())
    } else {
        Span::raw("")
    }
}

fn file_row(
    app: &App,
    fi: usize,
    selected: bool,
    max_churn: usize,
    w: u16,
    theme: &crate::theme::Theme,
) -> Line<'static> {
    let fc = &app.changeset.files[fi];
    let viewed = app.is_viewed(fi);
    let marked = app.ov.marked.contains(&fi);

    // Viewed dot — dim when viewed so unviewed pops.
    let dot = if viewed { "●" } else { "○" };
    let dot_style = if viewed {
        theme.dim()
    } else {
        Style::default().fg(theme.added)
    };

    // Counts column (fixed width 14): "+NNNN / -NNNN".
    let counts = vec![
        Span::styled(format!("+{:>4}", fc.additions), theme.added_style()),
        Span::styled(" / ", theme.dim()),
        Span::styled(format!("-{:>4}", fc.deletions), theme.removed_style()),
    ];

    // Size bar coloured by dominant change direction.
    let churn = fc.additions + fc.deletions;
    let bar_color = if fc.deletions > fc.additions {
        theme.removed
    } else {
        theme.added
    };
    let size_bar = bar(churn as f64 / max_churn as f64, SIZE_BAR_W, '█', '░');

    let annotation = annotation_for(fc, viewed, theme);

    // Fixed widths to align the columns.
    let fixed = 2 /*dot*/ + 2 /*status*/ + 14 /*counts*/ + 2 /*gap*/ + SIZE_BAR_W + 1 + 9 /*annot*/;
    let path_w = (w as usize).saturating_sub(fixed).max(8);
    let path = truncate_left(&fc.path.to_string_lossy(), path_w);

    let mut spans: Vec<Span> = vec![
        Span::raw(" "),
        Span::styled(dot.to_string(), dot_style),
        Span::raw(" "),
        Span::styled(
            fc.status.letter().to_string(),
            theme.status_style(fc.status),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:<width$}", path, width = path_w),
            Style::default(),
        ),
        Span::raw(" "),
    ];
    spans.extend(counts);
    spans.push(Span::raw("  "));
    spans.push(Span::styled(size_bar, Style::default().fg(bar_color)));
    spans.push(Span::raw(" "));
    if let Some((text, style)) = annotation {
        spans.push(Span::styled(format!("{text:<9}"), style));
    }

    let mut spans = pad_to(spans, w);
    // Marked rows get a leading marker by recolouring the first space.
    if marked {
        spans[0] = Span::styled("▎".to_string(), Style::default().fg(theme.accent));
    }

    let mut line = Line::from(spans);
    if selected {
        line = line.style(Style::default().bg(theme.bg_selected));
    }
    line
}

fn annotation_for(
    fc: &FileChange,
    viewed: bool,
    theme: &crate::theme::Theme,
) -> Option<(&'static str, Style)> {
    // Viewed takes visual precedence: it's the rarer, more useful signal while
    // reviewing.
    if viewed {
        return Some(("reviewed", theme.dim()));
    }
    if let Special::Binary { .. } = fc.special {
        return Some(("binary", theme.dim()));
    }
    fc.status.annotation().map(|a| {
        (
            a,
            Style::default().fg(theme.status_style(fc.status).fg.unwrap_or(theme.secondary)),
        )
    })
}

fn render_footer(app: &App, f: &mut Frame, area: Rect) {
    let theme = &app.theme;
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    f.render_widget(Paragraph::new(rule(area.width, theme)), parts[0]);
    let hints = [
        ("j/k", "nav"),
        ("⏎", "open"),
        ("v", "viewed"),
        ("/", "search"),
        ("s", "sort"),
        ("c", "commits"),
        ("?", "help"),
        ("q", "quit"),
    ];
    f.render_widget(
        Paragraph::new(footer(&hints, app.flash.as_deref(), theme)),
        parts[1],
    );
}
