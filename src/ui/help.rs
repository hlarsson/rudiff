//! Help overlay: a centered popup listing keybindings by context.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;

type Section = (&'static str, &'static [(&'static str, &'static str)]);

const SECTIONS: &[Section] = &[
    (
        "Global",
        &[
            ("q", "quit"),
            ("?", "toggle this help"),
            ("e", "explain changes with Claude"),
            ("esc", "close overlay / cancel / back"),
        ],
    ),
    (
        "Overview",
        &[
            ("j / k", "move down / up"),
            ("ctrl-d / ctrl-u", "half-page down / up"),
            ("gg / G", "top / bottom"),
            ("enter", "open file in diff view"),
            ("v", "toggle viewed on selection"),
            ("space", "multi-select file"),
            ("/", "filter file list"),
            ("s", "cycle sort (size/path/status)"),
            ("c", "commits view (placeholder)"),
        ],
    ),
    (
        "Diff view",
        &[
            ("j / k", "line down / up"),
            ("ctrl-d / ctrl-u", "half-page"),
            ("gg / G", "top / bottom"),
            ("]h / [h", "next / prev hunk"),
            ("]f / [f", "next / prev file"),
            ("]r / [r", "next / prev related file"),
            ("o", "back to overview"),
            ("v", "mark viewed, advance"),
            ("s", "toggle side-by-side / unified"),
            ("w", "cycle whitespace handling"),
            ("z / Z", "expand / collapse fold"),
            ("zR / zM", "expand / collapse all folds"),
            ("/ n N", "search, next, prev"),
        ],
    ),
];

pub fn draw(app: &App, f: &mut Frame) {
    let theme = &app.theme;
    let area = f.area();

    let mut lines: Vec<Line> = Vec::new();
    for (title, binds) in SECTIONS {
        lines.push(Line::from(Span::styled(
            (*title).to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *binds {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{key:<16}"), Style::default().fg(theme.added)),
                Span::styled((*desc).to_string(), Style::default().fg(theme.secondary)),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "press any key to close",
        theme.dim(),
    )));

    let height = (lines.len() as u16 + 2).min(area.height);
    let width = 52u16.min(area.width.saturating_sub(2));
    let popup = center(area, width, height);

    let block = Block::default()
        .title(" Keybindings ")
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
