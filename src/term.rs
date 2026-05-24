//! Terminal lifecycle: raw mode, alternate screen, the Kitty keyboard protocol
//! (where supported), and panic-safe restoration.

use std::io::{self, Stdout, Write};

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::crossterm::{execute, queue};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Set up the terminal and install a panic hook that restores it first, so a
/// crash never leaves the user's terminal in raw mode.
pub fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    // The Kitty keyboard protocol improves key disambiguation (e.g. distinct
    // Esc vs Alt, reliable Ctrl-modified keys). It degrades gracefully: if the
    // terminal doesn't advertise support we simply skip it.
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = queue!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }
    stdout.flush()?;

    install_panic_hook();

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state. Safe to call more than once.
pub fn restore() -> Result<()> {
    let mut stdout = io::stdout();
    // Popping the enhancement flags is harmless if we never pushed them.
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(stdout, LeaveAlternateScreen);
    disable_raw_mode()?;
    Ok(())
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        original(info);
    }));
}
