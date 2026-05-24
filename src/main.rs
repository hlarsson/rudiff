mod app;
mod cli;
mod config;
mod explain;
mod git;
mod group;
mod syntax;
mod term;
mod theme;
mod ui;
mod util;
mod viewed;

use std::process::ExitCode;

use clap::Parser;

use cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The TUI path restores the terminal itself (and the panic hook
            // covers crashes), so errors here are pre-init: just print the
            // top-level message, which carries our own clear context.
            eprintln!("rudiff: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let repo = git::Repo::discover(&cwd)?;
    let spec = git::RangeSpec::parse(cli.revspec.as_deref());
    let changeset = repo.build_changeset(&spec)?;
    let config = load_config(&cli, &cwd, repo.root());

    if cli.print {
        print_changeset(&repo, &changeset);
        return Ok(());
    }

    if let Some(dims) = &cli.snapshot {
        return snapshot(repo, changeset, &cli, config, dims);
    }

    let mut terminal = term::init()?;
    let result = app::App::new(repo, changeset, &cli, config).run(&mut terminal);
    term::restore()?;
    result
}

/// Resolve the active group config: none with `--no-config`, the `--config`
/// path if given, else discover `.rudiff.toml` walking up from cwd.
fn load_config(
    cli: &Cli,
    cwd: &std::path::Path,
    repo_root: &std::path::Path,
) -> Option<config::Config> {
    if cli.no_config {
        return None;
    }
    if let Some(path) = &cli.config {
        match config::Config::load_path(path) {
            Ok(c) => return Some(c),
            Err(e) => {
                eprintln!("rudiff: ignoring --config {}: {e}", path.display());
                return None;
            }
        }
    }
    config::Config::discover(cwd, repo_root)
}

/// Render one frame to an off-screen buffer and print it as text. Lets us
/// verify layout (and, with --keys, navigation) without a live terminal.
fn snapshot(
    repo: git::Repo,
    changeset: git::model::Changeset,
    cli: &Cli,
    config: Option<config::Config>,
    dims: &str,
) -> anyhow::Result<()> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (w, h) = dims
        .split_once('x')
        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
        .ok_or_else(|| anyhow::anyhow!("--snapshot expects WxH, e.g. 120x40"))?;

    let mut app = app::App::new(repo, changeset, cli, config);
    // Seed the width as if the overview had already rendered, so key-driven
    // mode decisions match real usage.
    app.width = w;
    if let Some(keys) = &cli.keys {
        app.feed_keys(keys);
    }

    // If a key script kicked off an async `claude` query, pump it to completion
    // (bounded) so the snapshot can capture the result rather than the spinner.
    let started = std::time::Instant::now();
    while app.explain.is_running() && started.elapsed() < std::time::Duration::from_secs(90) {
        app.explain.poll();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|f| ui::draw(&mut app, f))?;

    let buf = terminal.backend().buffer();
    let mut out = String::new();
    let mut fg_colors = std::collections::HashSet::new();
    let mut bg_colors = std::collections::HashSet::new();
    for y in 0..h {
        for x in 0..w {
            let cell = &buf[(x, y)];
            out.push_str(cell.symbol());
            fg_colors.insert(format!("{:?}", cell.fg));
            bg_colors.insert(format!("{:?}", cell.bg));
        }
        out.push('\n');
    }
    print!("{out}");
    // Diagnostic (stderr): distinct colors in the frame — confirms highlighting
    // and diff backgrounds are actually being applied.
    eprintln!(
        "[snapshot] {} distinct fg, {} distinct bg colors",
        fg_colors.len(),
        bg_colors.len()
    );
    Ok(())
}

/// Plain-text dump used by `--print` for testing and scripting.
fn print_changeset(repo: &git::Repo, cs: &git::model::Changeset) {
    use git::model::Special;
    println!("{} -> {}", cs.head_name, cs.base_name);
    println!(
        "{} files, +{} / -{}, {} commits, {} authors",
        cs.files.len(),
        cs.total_additions(),
        cs.total_deletions(),
        cs.commits.len(),
        cs.author_count(),
    );
    for f in &cs.files {
        let extra = match &f.special {
            Special::Binary { old_size, new_size } => format!("  [binary {old_size}->{new_size}]"),
            Special::Submodule => "  [submodule]".to_string(),
            Special::Symlink => "  [symlink]".to_string(),
            Special::None => String::new(),
        };
        let rename = f
            .old_path
            .as_ref()
            .map(|p| format!("  (from {})", p.display()))
            .unwrap_or_default();
        println!(
            "  {} {:<30} +{:<4} -{:<4} hash={:016x}{}{}",
            f.status.letter(),
            f.path.display(),
            f.additions,
            f.deletions,
            f.content_hash,
            rename,
            extra,
        );
    }
    // Verify lazy hunk loading on the first textual file.
    if let Some(f) = cs.files.iter().find(|f| f.special == Special::None) {
        let fd = repo.load_file_diff(f, git::diff::CONTEXT_RADIUS, false);
        println!(
            "\nfirst file `{}` -> {} hunk(s)",
            f.path.display(),
            fd.hunks.len()
        );
    }
}
