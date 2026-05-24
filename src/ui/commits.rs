//! Commits overlay — a read-only list of the commits in the range.
//!
//! This is the v1 placeholder for the deferred commit-by-commit view: it shows
//! *what* is in the range (and proves the data is there) without implementing
//! per-commit diffing.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::util::truncate_right;

pub fn draw(app: &App, f: &mut Frame) {
    let theme = &app.theme;
    let area = f.area();
    let commits = &app.changeset.commits;

    let width = 76u16.min(area.width.saturating_sub(2));
    let inner_w = width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Full commit-by-commit review lands in v2. For now, the range contains:",
        theme.dim(),
    )));
    lines.push(Line::from(""));

    // Show up to a screenful of commits (newest first, as walked).
    let max_rows = (area.height.saturating_sub(6) as usize).max(1);
    for c in commits.iter().take(max_rows) {
        let short = c.id.to_string().chars().take(8).collect::<String>();
        let author = truncate_right(&c.author, 16);
        let summary_w = inner_w.saturating_sub(8 + 1 + 16 + 2 + 2);
        let summary = truncate_right(&c.summary, summary_w.max(8));
        lines.push(Line::from(vec![
            Span::styled(short, Style::default().fg(theme.modified)),
            Span::raw("  "),
            Span::styled(format!("{author:<16}"), theme.dim()),
            Span::raw("  "),
            Span::raw(summary),
        ]));
    }
    if commits.len() > max_rows {
        lines.push(Line::from(Span::styled(
            format!("… and {} more", commits.len() - max_rows),
            theme.dim(),
        )));
    }
    if commits.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no commits in range)",
            theme.dim(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press any key to close",
        theme.dim(),
    )));

    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = center(area, width, height);
    let title = format!(" Commits ({}) ", commits.len());
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(theme.chrome_style());

    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn center(area: Rect, width: u16, height: u16) -> Rect {
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(h);
    v
}
