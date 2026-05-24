//! Minimal Markdown → ratatui rendering for the explain response window.
//!
//! Deliberately small: headings, bullet/numbered lists, blockquotes, fenced
//! code blocks, and inline `**bold**`, `*italic*`, and `` `code` ``. It is a
//! best-effort prettifier for `claude`'s output (which is partial while
//! streaming), not a spec-compliant parser — unmatched markers render
//! literally and nothing ever errors.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Render Markdown `text` into styled lines.
pub fn render(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let body = Style::default().fg(theme.secondary);
    let mut out = Vec::new();
    let mut in_code = false;

    for raw in text.lines() {
        let trimmed = raw.trim_start();

        // Fenced code block: toggle, and don't render the ``` marker itself.
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                format!("  {raw}"),
                body.bg(theme.bg_selected),
            )));
            continue;
        }

        // Heading: `#`..`######` + space.
        if let Some(rest) = heading(trimmed) {
            out.push(Line::from(Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }

        // Blockquote.
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("┃ ".to_string(), theme.dim())];
            spans.extend(inline(rest, body.add_modifier(Modifier::ITALIC), theme));
            out.push(Line::from(spans));
            continue;
        }

        // Bullet list (preserving indentation), with a tidy bullet glyph.
        if let Some((indent, rest)) = bullet(raw) {
            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled("• ".to_string(), Style::default().fg(theme.accent)),
            ];
            spans.extend(inline(rest, body, theme));
            out.push(Line::from(spans));
            continue;
        }

        // Everything else (incl. numbered lists) — just inline formatting.
        out.push(Line::from(inline(raw, body, theme)));
    }
    out
}

/// Strip a leading `#`..`######` + space, returning the heading text.
fn heading(trimmed: &str) -> Option<&str> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        trimmed[hashes..].strip_prefix(' ')
    } else {
        None
    }
}

/// Detect a `- `/`* `/`+ ` bullet, returning (leading indent, item text).
fn bullet(raw: &str) -> Option<(usize, &str)> {
    let indent = raw.len() - raw.trim_start().len();
    let trimmed = &raw[indent..];
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some((indent, rest));
        }
    }
    None
}

/// Parse inline `**bold**`, `*italic*`, and `` `code` `` over `base` style.
fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let code_style = base.bg(theme.bg_selected);
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), base));
        }
    };

    while i < n {
        let c = chars[i];
        // `code`
        if c == '`'
            && let Some(close) = (i + 1..n).find(|&j| chars[j] == '`')
        {
            flush(&mut buf, &mut spans);
            spans.push(Span::styled(
                chars[i + 1..close].iter().collect::<String>(),
                code_style,
            ));
            i = close + 1;
            continue;
        }
        // **bold**
        if c == '*'
            && i + 1 < n
            && chars[i + 1] == '*'
            && let Some(close) = find_double_star(&chars, i + 2)
        {
            flush(&mut buf, &mut spans);
            spans.push(Span::styled(
                chars[i + 2..close].iter().collect::<String>(),
                base.add_modifier(Modifier::BOLD),
            ));
            i = close + 2;
            continue;
        }
        // *italic* (a lone star, not part of `**`)
        if c == '*'
            && (i + 1 >= n || chars[i + 1] != '*')
            && let Some(close) = (i + 1..n).find(|&j| chars[j] == '*')
        {
            flush(&mut buf, &mut spans);
            spans.push(Span::styled(
                chars[i + 1..close].iter().collect::<String>(),
                base.add_modifier(Modifier::ITALIC),
            ));
            i = close + 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// Index of the next `**` starting at or after `from`.
fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == '*' && chars[j + 1] == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn formats_common_markdown() {
        let theme = Theme::detect();
        let md = "## Heading\n- a **bold** item\n`code` here\n> quote\nplain";
        let lines = render(md, &theme);
        assert_eq!(plain(&lines[0]), "Heading"); // `##` stripped
        assert!(plain(&lines[1]).starts_with("• ")); // bullet glyph
        assert!(plain(&lines[1]).contains("bold")); // bold text preserved
        assert!(plain(&lines[2]).contains("code"));
        assert!(plain(&lines[3]).contains("quote"));
        assert_eq!(plain(&lines[4]), "plain");
    }

    #[test]
    fn code_fence_hides_markers_and_styles_body() {
        let theme = Theme::detect();
        let lines = render("before\n```\nlet x = 1;\n```\nafter", &theme);
        let texts: Vec<String> = lines.iter().map(plain).collect();
        assert!(texts.iter().all(|t| !t.contains("```")));
        assert!(texts.iter().any(|t| t.contains("let x = 1;")));
    }

    #[test]
    fn unmatched_markers_render_literally() {
        let theme = Theme::detect();
        let lines = render("a **bold that never closes", &theme);
        // No panic; the text is preserved (possibly across spans).
        assert!(plain(&lines[0]).contains("bold that never closes"));
    }
}
