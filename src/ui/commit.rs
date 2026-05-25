//! Overlay for committing the reviewed files: a single commit-message prompt.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::commit::Commit;
use crate::theme::Theme;
use crate::util::plural;

pub fn draw(app: &App, f: &mut Frame) {
    let theme = &app.theme;
    let area = f.area();
    let Commit::Prompting(p) = &app.commit else {
        return;
    };
    // In the uncommitted view the changeset's base name is the current branch.
    let branch = app.changeset.base_name.clone();
    prompting(
        theme,
        f,
        area,
        &p.input,
        p.files.len(),
        &branch,
        p.notice.as_deref(),
    );
}

fn prompting(
    theme: &Theme,
    f: &mut Frame,
    area: Rect,
    input: &str,
    count: usize,
    branch: &str,
    notice: Option<&str>,
) {
    let width = 72u16.min(area.width.saturating_sub(2));
    let popup = center(area, width, 8);
    let inner_w = popup.width.saturating_sub(2) as usize;

    // Show the tail of the message if it's longer than the field, with a cursor.
    let field_w = inner_w.saturating_sub(4);
    let shown = crate::util::truncate_left(input, field_w);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Commit ", Style::default().fg(theme.secondary)),
            Span::styled(
                plural(count, "reviewed file"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to ", Style::default().fg(theme.secondary)),
            Span::styled(
                branch.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled("Enter a commit message:", theme.dim())),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::raw(shown),
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]),
        Line::from(""),
    ];
    // A validation/error notice replaces the hint until the next keystroke.
    if let Some(n) = notice {
        lines.push(Line::from(Span::styled(
            n.to_string(),
            theme.removed_style(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "enter to commit · esc to cancel",
            theme.dim(),
        )));
    }

    let block = Block::default()
        .title(" Commit reviewed files ")
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
