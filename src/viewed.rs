//! Viewed-status tracking, keyed by the per-file diff content hash.
//!
//! Persistence writes to `<git_dir>/rudiff/viewed.json`. Keying on the content
//! hash rather than path/SHA is the whole point: a file stays "viewed" across
//! rebases that don't change its diff, and silently un-views when its diff
//! meaningfully changes.
//!
//! Hashes are stored as zero-padded hex strings, not JSON numbers, to dodge the
//! 2^53 precision limit of JSON number parsers and to keep the file
//! greppable/diff-friendly. The whole set is retained (never pruned to the
//! current changeset) so switching branches doesn't drop viewed marks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Default)]
struct OnDisk {
    version: u32,
    /// Content hashes as 16-char hex strings.
    viewed: Vec<String>,
}

pub struct Viewed {
    set: HashSet<u64>,
    /// Where to persist; `None` disables saving (in-memory mode / tests).
    path: Option<PathBuf>,
    dirty: bool,
}

impl Viewed {
    /// Load (or initialize) the viewed set for a repository's git dir. A
    /// missing or corrupt file is treated as empty — viewed status is a
    /// convenience, never a hard dependency.
    pub fn load(git_dir: &Path) -> Viewed {
        let path = git_dir.join("rudiff").join("viewed.json");
        let set = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<OnDisk>(&bytes).ok())
            .map(|d| {
                d.viewed
                    .iter()
                    .filter_map(|s| u64::from_str_radix(s, 16).ok())
                    .collect()
            })
            .unwrap_or_default();
        Viewed {
            set,
            path: Some(path),
            dirty: false,
        }
    }

    pub fn is_viewed(&self, hash: u64) -> bool {
        self.set.contains(&hash)
    }

    pub fn set_viewed(&mut self, hash: u64, viewed: bool) {
        let changed = if viewed {
            self.set.insert(hash)
        } else {
            self.set.remove(&hash)
        };
        self.dirty |= changed;
    }

    /// Persist if there are unsaved changes. Writes atomically (temp file +
    /// rename) so an interrupted write can't corrupt the existing file. Errors
    /// are swallowed: failing to save viewed status must never crash the tool.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };
        if self.write(path).is_ok() {
            self.dirty = false;
        }
    }

    fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut hashes: Vec<String> = self.set.iter().map(|h| format!("{h:016x}")).collect();
        hashes.sort_unstable(); // deterministic file (nice for git/inspection)
        let data = OnDisk {
            version: FILE_VERSION,
            viewed: hashes,
        };
        let json = serde_json::to_vec_pretty(&data).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("rudiff-viewed-test-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();

        {
            let mut v = Viewed::load(&git_dir);
            v.set_viewed(0xdead_beef_cafe_babe, true);
            v.set_viewed(0x0000_0000_0000_0001, true);
            v.save();
        }
        let v2 = Viewed::load(&git_dir);
        assert!(v2.is_viewed(0xdead_beef_cafe_babe));
        assert!(v2.is_viewed(1));
        assert!(!v2.is_viewed(2));

        std::fs::remove_dir_all(&dir).ok();
    }
}
