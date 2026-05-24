//! Rendering. Pure-ish functions that take the [`App`] and a ratatui `Frame`.
//! Layout-time mutations (clamping scroll, recording viewport height) happen
//! here because they depend on the rendered area.

mod commits;
mod diffview;
mod explain;
mod help;
mod markdown;
mod overview;
pub mod widgets;

use ratatui::Frame;

use crate::app::{App, Screen};

pub fn draw(app: &mut App, f: &mut Frame) {
    app.width = f.area().width;
    // Auto-switch layout on resize unless the user pinned a mode.
    app.reconcile_mode();
    match app.screen {
        Screen::Overview => overview::draw(app, f),
        Screen::Diff => diffview::draw(app, f),
    }
    if app.show_help {
        help::draw(app, f);
    } else if app.show_commits {
        commits::draw(app, f);
    } else if app.explain.is_active() {
        explain::draw(app, f);
    }
}
