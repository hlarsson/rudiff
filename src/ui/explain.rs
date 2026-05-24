//! Overlay for the "explain these changes" feature: a spinner while `claude`
//! runs, then the wrapped, scrollable response.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::explain::Explain;
use crate::theme::Theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    let theme = app.theme; // `Theme` is Copy, so this doesn't borrow `app`.
    match &mut app.explain {
        Explain::Idle => {}
        Explain::Prompting(p) => prompting(&theme, f, area, &p.target, &p.input),
        Explain::Running(r) => {
            spinner(&theme, f, area, &r.target, r.started.elapsed().as_millis());
        }
        Explain::Result {
            text,
            is_error,
            scroll,
        } => {
            let (popup, body_h, inner_w) = result_layout(area);
            // Clamp scroll to the wrapped content height now that we know the
            // body width (so we never scroll into the void).
            let wrapped = wrapped_line_count(text, inner_w);
            let max_scroll = wrapped.saturating_sub(body_h);
            if *scroll > max_scroll {
                *scroll = max_scroll;
            }
            result(&theme, f, popup, text, *is_error, *scroll);
        }
    }
}

fn prompting(theme: &Theme, f: &mut Frame, area: Rect, target: &str, input: &str) {
    let width = 72u16.min(area.width.saturating_sub(2));
    let popup = center(area, width, 7);
    let inner_w = popup.width.saturating_sub(2) as usize;

    // Show the tail of the input if it's longer than the field, with a cursor.
    let field_w = inner_w.saturating_sub(4); // "> " + cursor headroom
    let shown = crate::util::truncate_left(input, field_w);

    let lines = vec![
        Line::from(vec![
            Span::styled("Explain ", Style::default().fg(theme.secondary)),
            Span::styled(
                target.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            "Add guidance (optional), e.g. \"focus on edge cases\":",
            theme.dim(),
        )),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::raw(shown),
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]),
        Line::from(""),
        Line::from(Span::styled("enter to ask · esc to cancel", theme.dim())),
    ];

    let block = Block::default()
        .title(" Explain with Claude ")
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

fn spinner(theme: &Theme, f: &mut Frame, area: Rect, target: &str, elapsed_ms: u128) {
    let frame = SPINNER[(elapsed_ms / 90) as usize % SPINNER.len()];
    let secs = elapsed_ms / 1000;

    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{frame} "), Style::default().fg(theme.accent)),
            Span::styled(
                "Asking Claude to explain ",
                Style::default().fg(theme.secondary),
            ),
            Span::styled(
                target.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  ({secs}s)"), theme.dim()),
        ]),
        Line::from(""),
        Line::from(Span::styled("press esc to cancel", theme.dim())),
    ];

    let width = 64u16.min(area.width.saturating_sub(2));
    let popup = center(area, width, 5);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.chrome_style());
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Popup rect, body height (lines), and inner content width for the result.
fn result_layout(area: Rect) -> (Rect, usize, usize) {
    let width = 84u16.min(area.width.saturating_sub(4));
    let height = ((area.height as f32 * 0.8) as u16).max(6);
    let popup = center(area, width, height);
    let inner_w = popup.width.saturating_sub(2) as usize; // borders
    let body_h = popup.height.saturating_sub(3) as usize; // borders + footer
    (popup, body_h, inner_w.max(1))
}

fn result(theme: &Theme, f: &mut Frame, popup: Rect, text: &str, is_error: bool, scroll: usize) {
    let (title, title_color) = if is_error {
        (" Explain — error ", theme.removed)
    } else {
        (" Explanation ", theme.accent)
    };
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(theme.chrome_style());
    let inner = block.inner(popup);

    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let body: Vec<Line> = text
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme.secondary),
            ))
        })
        .collect();

    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        parts[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "j/k scroll · esc/q close",
            theme.dim(),
        ))),
        parts[1],
    );
}

/// Estimate how many terminal rows `text` occupies when wrapped to `width`.
fn wrapped_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    text.lines()
        .map(|l| {
            let w = l.width();
            if w == 0 { 1 } else { w.div_ceil(width) }
        })
        .sum()
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
