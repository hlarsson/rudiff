//! `.rudiff.toml` discovery and parsing.
//!
//! ```toml
//! [[group]]
//! name = "Auth"
//! patterns = ["server/src/main/java/com/acme/auth/**", "clients/*/src/auth/**"]
//! ```
//!
//! Patterns are gitignore-style globs matched against repo-root-relative paths.
//! Groups are intentionally flat (no nesting) — they model vertical domain
//! slices, not layers. A file may match several groups (it then appears under
//! each); unmatched files fall under "Other".

use std::path::Path;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default)]
    group: Vec<RawGroup>,
}

#[derive(Deserialize)]
struct RawGroup {
    name: String,
    #[serde(default)]
    patterns: Vec<String>,
}

pub struct Config {
    groups: Vec<GroupMatcher>,
}

struct GroupMatcher {
    name: String,
    set: GlobSet,
}

impl Config {
    /// Walk up from `start` (typically cwd) to `repo_root` inclusive, returning
    /// the first `.rudiff.toml` parsed. `None` if none is found.
    pub fn discover(start: &Path, repo_root: &Path) -> Option<Config> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(".rudiff.toml");
            if candidate.is_file() {
                // A malformed config shouldn't be silently ignored, but it also
                // shouldn't crash the viewer; surface it on stderr and fall back.
                match Config::load_path(&candidate) {
                    Ok(cfg) => return Some(cfg),
                    Err(e) => {
                        eprintln!("rudiff: ignoring {}: {e}", candidate.display());
                        return None;
                    }
                }
            }
            if d == repo_root {
                break;
            }
            dir = d.parent();
        }
        None
    }

    pub fn load_path(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Config::parse(&text)
    }

    fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text).context("invalid TOML")?;
        let mut groups = Vec::with_capacity(raw.group.len());
        for g in raw.group {
            let mut builder = GlobSetBuilder::new();
            for pat in &g.patterns {
                // `literal_separator(true)` gives gitignore semantics: `*`
                // stops at `/`, `**` crosses directories.
                let glob = Glob::new(pat)
                    .with_context(|| format!("invalid glob `{pat}` in group `{}`", g.name))?;
                let glob = globset::GlobBuilder::new(glob.glob())
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("invalid glob `{pat}` in group `{}`", g.name))?;
                builder.add(glob);
            }
            let set = builder.build().context("failed to build glob set")?;
            groups.push(GroupMatcher { name: g.name, set });
        }
        Ok(Config { groups })
    }

    /// Indices of the groups a path belongs to (empty => "Other").
    pub fn groups_for(&self, path: &Path) -> Vec<usize> {
        self.groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.set.is_match(path))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn group_names(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().map(|g| g.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn matches_gitignore_style_globs() {
        let cfg = Config::parse(
            r#"
            [[group]]
            name = "Auth"
            patterns = ["server/auth/**", "clients/*/auth/**"]

            [[group]]
            name = "Billing"
            patterns = ["server/billing/**"]
            "#,
        )
        .unwrap();

        assert_eq!(
            cfg.groups_for(&PathBuf::from("server/auth/session.rs")),
            vec![0]
        );
        assert_eq!(
            cfg.groups_for(&PathBuf::from("clients/web/auth/login.ts")),
            vec![0]
        );
        assert_eq!(
            cfg.groups_for(&PathBuf::from("server/billing/invoice.rs")),
            vec![1]
        );
        // `*` shouldn't cross a directory separator.
        assert!(
            cfg.groups_for(&PathBuf::from("clients/web/deep/auth/x.ts"))
                .is_empty()
        );
        // Unmatched.
        assert!(cfg.groups_for(&PathBuf::from("README.md")).is_empty());
    }

    #[test]
    fn file_can_match_multiple_groups() {
        let cfg = Config::parse(
            r#"
            [[group]]
            name = "A"
            patterns = ["src/**"]
            [[group]]
            name = "B"
            patterns = ["**/auth/**"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.groups_for(&PathBuf::from("src/auth/x.rs")), vec![0, 1]);
    }
}
