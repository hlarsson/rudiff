//! Command-line interface (clap).

use clap::Parser;

/// A fast, read-only terminal viewer for git diffs — built for reviewing
/// branches the way you'd review a pull request.
#[derive(Parser, Debug)]
#[command(name = "rudiff", version, about, long_about = None)]
pub struct Cli {
    /// What to compare. Examples:
    ///   (omitted)        default branch vs HEAD
    ///   main             merge-base(main, HEAD) vs HEAD  (PR semantics)
    ///   main..feature    direct diff main → feature
    ///   main...feature   merge-base diff (PR semantics)
    ///   abc123           merge-base(abc123, HEAD) vs HEAD
    #[arg(value_name = "REVSPEC", verbatim_doc_comment)]
    pub revspec: Option<String>,

    /// Review uncommitted changes: the working tree vs HEAD (like `git diff
    /// HEAD`). Shows staged and unstaged edits to tracked files; untracked
    /// files are not included. Cannot be combined with a REVSPEC.
    #[arg(short = 'u', long, conflicts_with = "revspec")]
    pub uncommitted: bool,

    /// Force unified (single-column) layout.
    #[arg(long, conflicts_with = "side_by_side")]
    pub unified: bool,

    /// Force side-by-side (two-column) layout.
    #[arg(long = "side-by-side")]
    pub side_by_side: bool,

    /// Ignore any `.rudiff.toml`; group changes by directory instead.
    #[arg(long)]
    pub no_config: bool,

    /// Use a specific config file instead of discovering `.rudiff.toml`.
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,

    /// Print the changeset summary as plain text and exit (no TUI).
    #[arg(long, hide = true)]
    pub print: bool,

    /// Render one frame to an off-screen buffer of WxH (e.g. 120x40), print it,
    /// and exit. For development/testing.
    #[arg(long, hide = true, value_name = "WxH")]
    pub snapshot: Option<String>,

    /// With --snapshot, drive the app with a key script first (e.g. "j,j,CR").
    #[arg(long, hide = true, value_name = "KEYS")]
    pub keys: Option<String>,
}

/// The display mode the user forced on the command line, if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForcedMode {
    Unified,
    SideBySide,
}

impl Cli {
    pub fn forced_mode(&self) -> Option<ForcedMode> {
        match (self.unified, self.side_by_side) {
            (true, _) => Some(ForcedMode::Unified),
            (_, true) => Some(ForcedMode::SideBySide),
            _ => None,
        }
    }
}
