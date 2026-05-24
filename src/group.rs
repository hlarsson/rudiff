//! Rollup of file changes into named groups for the overview.
//!
//! Phase 3 implements directory-based grouping (the silent fallback when there
//! is no `.rudiff.toml`). Phase 11 adds config-driven groups; both produce the
//! same [`Group`] shape so the renderer doesn't care which is in use.

use crate::git::model::FileChange;

#[derive(Clone, Debug)]
pub struct Group {
    pub name: String,
    /// Distinct files counted under this group.
    pub file_count: usize,
    pub additions: usize,
    pub deletions: usize,
}

impl Group {
    pub fn churn(&self) -> usize {
        self.additions + self.deletions
    }
}

/// Result of grouping: the rollup plus whether any file landed in more than
/// one group (which drives the "counted once" footnote).
pub struct Grouping {
    pub groups: Vec<Group>,
    pub from_config: bool,
    pub multi_group: bool,
}

/// Build the rollup from an optional config. Falls back to directory grouping
/// when there is no config.
pub fn build(files: &[FileChange], config: Option<&crate::config::Config>) -> Grouping {
    match config {
        Some(cfg) => by_config(files, cfg),
        None => Grouping {
            groups: by_directory(files),
            from_config: false,
            multi_group: false,
        },
    }
}

/// Group files by the configured `.rudiff.toml` groups. A file matching several
/// groups is counted under each; unmatched files go to "Other". Totals in the
/// overview come from the changeset (not group sums), so multi-group files
/// still count once overall.
fn by_config(files: &[FileChange], config: &crate::config::Config) -> Grouping {
    let names: Vec<String> = config.group_names().map(str::to_string).collect();
    let mut groups: Vec<Group> = names
        .iter()
        .map(|name| Group {
            name: name.clone(),
            file_count: 0,
            additions: 0,
            deletions: 0,
        })
        .collect();
    let mut other = Group {
        name: "Other".to_string(),
        file_count: 0,
        additions: 0,
        deletions: 0,
    };
    let mut multi_group = false;

    for f in files {
        let matched = config.groups_for(&f.path);
        multi_group |= matched.len() > 1;
        if matched.is_empty() {
            other.file_count += 1;
            other.additions += f.additions;
            other.deletions += f.deletions;
        } else {
            for gi in matched {
                groups[gi].file_count += 1;
                groups[gi].additions += f.additions;
                groups[gi].deletions += f.deletions;
            }
        }
    }

    // Drop empty configured groups; keep "Other" only if non-empty.
    groups.retain(|g| g.file_count > 0);
    if other.file_count > 0 {
        groups.push(other);
    }
    Grouping {
        groups: sort_groups(groups),
        from_config: true,
        multi_group,
    }
}

/// Group files by their parent directory (e.g. `src/auth`). Root-level files
/// fall under `(root)`. Sorted by churn descending, then name.
pub fn by_directory(files: &[FileChange]) -> Vec<Group> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, Group> = BTreeMap::new();
    for f in files {
        let dir = f
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(root)".to_string());
        let g = map.entry(dir.clone()).or_insert_with(|| Group {
            name: dir,
            file_count: 0,
            additions: 0,
            deletions: 0,
        });
        g.file_count += 1;
        g.additions += f.additions;
        g.deletions += f.deletions;
    }
    sort_groups(map.into_values().collect())
}

/// Sort groups by churn (desc) then name (asc), keeping any "Other"/"(root)"
/// bucket last so it doesn't dominate the top of the list.
pub fn sort_groups(mut groups: Vec<Group>) -> Vec<Group> {
    groups.sort_by(|a, b| {
        let a_other = is_catchall(&a.name);
        let b_other = is_catchall(&b.name);
        a_other
            .cmp(&b_other)
            .then_with(|| b.churn().cmp(&a.churn()))
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

fn is_catchall(name: &str) -> bool {
    name == "Other" || name == "(root)"
}

/// The largest churn across groups, used to scale the rollup bars.
pub fn max_churn(groups: &[Group]) -> usize {
    groups.iter().map(Group::churn).max().unwrap_or(0)
}
