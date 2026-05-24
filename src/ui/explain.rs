//! Overlay for the "explain these changes" feature: a guidance prompt, then the
//! response streaming in live, then the final scrollable result.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::explain::{Explain, ExplainModel};
use crate::theme::Theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    let theme = app.theme; // `Theme` is Copy, so this doesn't borrow `app`.
    match &mut app.explain {
        Explain::Idle => {}
        Explain::Prompting(p) => prompting(&theme, f, area, &p.target, &p.input, p.model),
        Explain::Running(r) => {
            let elapsed = r.started.elapsed().as_millis();
            let frame = SPINNER[(elapsed / 90) as usize % SPINNER.len()];
            let secs = elapsed / 1000;
            let model = r
                .model
                .map(|m| format!("{} · ", m.alias()))
                .unwrap_or_default();
            let title = format!(" {frame} Explaining {} · {model}{secs}s ", r.target);
            let lines = if r.partial.is_empty() {
                vec![Line::from(Span::styled("Waiting for Claude…", theme.dim()))]
            } else {
                crate::ui::markdown::render(&r.partial, &theme)
            };
            // Stream view auto-scrolls to the bottom so the newest text shows.
            text_panel(
                &theme,
                f,
                area,
                &title,
                theme.accent,
                lines,
                "esc to cancel",
                None,
            );
        }
        Explain::Result {
            text,
            is_error,
            scroll,
            save,
            notice,
            ..
        } => {
            // While editing the save filename, that input takes over.
            if let Some(filename) = save {
                save_prompt(&theme, f, area, filename);
                return;
            }
            let (title, color) = if *is_error {
                (" Explain — error ".to_string(), theme.removed)
            } else {
                (" Explanation ".to_string(), theme.accent)
            };
            let lines = crate::ui::markdown::render(text, &theme);
            // Clamp the stored scroll to the rendered content, then render.
            let (_, body_h, inner_w) = result_layout(area);
            let max = wrapped_count(&lines, inner_w).saturating_sub(body_h);
            if *scroll > max {
                *scroll = max;
            }
            // A save confirmation (if any) replaces the hints in the footer.
            let footer = notice
                .clone()
                .unwrap_or_else(|| "j/k scroll · s save · esc/q close".to_string());
            text_panel(
                &theme,
                f,
                area,
                &title,
                color,
                lines,
                &footer,
                Some(*scroll),
            );
        }
    }
}

fn prompting(
    theme: &Theme,
    f: &mut Frame,
    area: Rect,
    target: &str,
    input: &str,
    model: Option<ExplainModel>,
) {
    let width = 72u16.min(area.width.saturating_sub(2));
    let popup = center(area, width, 7);
    let inner_w = popup.width.saturating_sub(2) as usize;

    // Show the tail of the input if it's longer than the field, with a cursor.
    let field_w = inner_w.saturating_sub(4);
    let shown = crate::util::truncate_left(input, field_w);

    // First line: "Explain <target>" plus the model tag when configured.
    let mut header = vec![
        Span::styled("Explain ", Style::default().fg(theme.secondary)),
        Span::styled(
            target.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(m) = model {
        header.push(Span::styled(format!("   ({})", m.alias()), theme.dim()));
    }

    let lines = vec![
        Line::from(header),
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

/// The "save to file" input popup (filename pre-filled, editable).
fn save_prompt(theme: &Theme, f: &mut Frame, area: Rect, filename: &str) {
    let width = 76u16.min(area.width.saturating_sub(2));
    let popup = center(area, width, 6);
    let inner_w = popup.width.saturating_sub(2) as usize;
    let shown = crate::util::truncate_left(filename, inner_w.saturating_sub(4));

    let lines = vec![
        Line::from(Span::styled(
            "Save the explanation to a Markdown file:",
            Style::default().fg(theme.secondary),
        )),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::raw(shown),
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]),
        Line::from(""),
        Line::from(Span::styled("enter to save · esc to cancel", theme.dim())),
    ];

    let block = Block::default()
        .title(" Save explanation ")
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

/// Render a bordered, wrapped, scrollable text panel from pre-styled lines.
/// `scroll = None` pins the view to the bottom (used while streaming);
/// `Some(n)` scrolls to wrapped line `n`.
#[allow(clippy::too_many_arguments)] // a self-contained renderer; a struct would just add noise
fn text_panel(
    theme: &Theme,
    f: &mut Frame,
    area: Rect,
    title: &str,
    title_color: ratatui::style::Color,
    lines: Vec<Line<'static>>,
    footer: &str,
    scroll: Option<usize>,
) {
    let (popup, body_h, inner_w) = result_layout(area);
    let max = wrapped_count(&lines, inner_w).saturating_sub(body_h);
    let scroll = scroll.unwrap_or(max).min(max);

    let block = Block::default()
        .title(title.to_string())
        .title_style(
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(theme.chrome_style());
    let inner = block.inner(popup);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    f.render_widget(Clear, popup);
    f.render_widget(block, popup);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        parts[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(footer.to_string(), theme.dim()))),
        parts[1],
    );
}

/// Popup rect, body height (lines), and inner content width.
fn result_layout(area: Rect) -> (Rect, usize, usize) {
    let width = 84u16.min(area.width.saturating_sub(4));
    let height = ((area.height as f32 * 0.8) as u16).max(6);
    let popup = center(area, width, height);
    let inner_w = popup.width.saturating_sub(2) as usize;
    let body_h = popup.height.saturating_sub(3) as usize; // borders + footer
    (popup, body_h, inner_w.max(1))
}

/// Estimate how many terminal rows the rendered `lines` occupy at `width`.
fn wrapped_count(lines: &[Line], width: usize) -> usize {
    let width = width.max(1);
    lines
        .iter()
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
