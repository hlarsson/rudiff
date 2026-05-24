//! Small formatting/rendering helpers shared across screens.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Format an integer with thousands separators, e.g. `1247` -> `1,247`.
pub fn commas(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Pluralize a count: `plural(1, "author")` -> `1 author`, `plural(3, ...)` ->
/// `3 authors`.
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Render a relative age like `4d ago`, `3h ago`, `just now` from a unix
/// timestamp and the current time.
pub fn relative_age(then_secs: i64, now_secs: i64) -> String {
    let d = now_secs.saturating_sub(then_secs);
    if d < 0 {
        return "in the future".to_string();
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    if d < MIN {
        "just now".to_string()
    } else if d < HOUR {
        format!("{}m ago", d / MIN)
    } else if d < DAY {
        format!("{}h ago", d / HOUR)
    } else if d < WEEK {
        format!("{}d ago", d / DAY)
    } else if d < MONTH {
        format!("{}w ago", d / WEEK)
    } else if d < YEAR {
        format!("{}mo ago", d / MONTH)
    } else {
        format!("{}y ago", d / YEAR)
    }
}

/// Human-readable byte size, e.g. `1.2 MB`, `840 B`.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// A solid/empty bar of the given cell width filled to `ratio` (0.0..=1.0),
/// using the supplied fill and empty glyphs.
pub fn bar(ratio: f64, width: usize, fill: char, empty: char) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push(fill);
    }
    for _ in 0..(width - filled) {
        s.push(empty);
    }
    s
}

/// Truncate a string to a maximum display width, keeping the **tail** (so a
/// path keeps its filename) and prefixing an ellipsis when cut.
pub fn truncate_left(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width.saturating_sub(1); // room for the ellipsis
    // Walk from the right accumulating width until we hit the budget.
    let mut width = 0;
    let mut start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let w = ch.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        start = idx;
    }
    format!("…{}", &s[start..])
}

/// Truncate to a maximum display width keeping the **head**, appending an
/// ellipsis when cut.
pub fn truncate_right(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width.saturating_sub(1);
    let mut width = 0;
    let mut end = 0;
    for (idx, ch) in s.char_indices() {
        let w = ch.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        end = idx + ch.len_utf8();
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas_works() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1247), "1,247");
        assert_eq!(commas(1234567), "1,234,567");
    }

    #[test]
    fn truncate_left_keeps_tail() {
        assert_eq!(
            truncate_left("src/auth/session.rs", 100),
            "src/auth/session.rs"
        );
        let t = truncate_left("src/auth/session.rs", 12);
        assert!(t.starts_with('…'));
        assert!(t.ends_with("session.rs"));
        assert!(t.chars().count() <= 12);
    }

    #[test]
    fn bar_fills() {
        assert_eq!(bar(0.0, 4, '#', '.'), "....");
        assert_eq!(bar(1.0, 4, '#', '.'), "####");
        assert_eq!(bar(0.5, 4, '#', '.'), "##..");
    }
}
