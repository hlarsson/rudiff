//! Shared rendering helpers: rules, header/footer bars, and line padding.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// A full-width horizontal rule.
pub fn rule(width: u16, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        theme.chrome_style(),
    ))
}

/// A line with `left` content and `right` content pushed to the far right,
/// padded with spaces to `width`.
pub fn justified(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let lw: usize = left.iter().map(|s| s.content.width()).sum();
    let rw: usize = right.iter().map(|s| s.content.width()).sum();
    let gap = (width as usize).saturating_sub(lw + rw);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Line::from(spans)
}

/// Pad a set of spans with trailing spaces so the line fills `width` cells.
/// Used so a row background covers the whole row.
pub fn pad_to(mut spans: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    let pad = (width as usize).saturating_sub(used);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans
}

/// The footer key-hint bar: a leading rule then a dim hint line.
pub fn footer<'a>(
    hints: &[(&'a str, &'a str)],
    flash: Option<&str>,
    theme: &Theme,
) -> Line<'static> {
    if let Some(msg) = flash {
        return Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(theme.accent),
        ));
    }
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", theme.dim()));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::default().fg(theme.accent),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(desc.to_string(), theme.dim()));
    }
    Line::from(spans)
}
