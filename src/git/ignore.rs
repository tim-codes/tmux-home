//! ~/.strayignore: paths and path-prefixes to hide from the inventory.
//!
//! Vendored from stray (https://github.com/tim-codes/stray, `src/ignore.rs`
//! at v0.3.0; MIT, Copyright (c) 2026 Tim O'Connell — see NOTICE),
//! unchanged but for module paths. It edits `~/.strayignore` (the user's
//! file, shared with stray), never a repository.
//!
//! One entry per line; `#` starts a comment. An entry matches a repo when it
//! equals — or is a parent directory of — either the repo's absolute path
//! (entries starting with `/` or `~`) or its name relative to the scan root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::model::display_path;

pub fn ignore_file() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".strayignore")
}

pub fn load() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(ignore_file()) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn save(entries: &[String]) -> Result<()> {
    let path = ignore_file();
    let mut text = entries.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// Add `entry` if missing. Returns false if it was already present.
pub fn add(entry: &str) -> Result<bool> {
    let mut entries = load();
    if entries.iter().any(|e| e == entry) {
        return Ok(false);
    }
    entries.push(entry.to_string());
    save(&entries)?;
    Ok(true)
}

/// Remove every entry that equals `entry`. Returns false if none matched.
pub fn remove(entry: &str) -> Result<bool> {
    let mut entries = load();
    let before = entries.len();
    entries.retain(|e| e != entry);
    if entries.len() == before {
        return Ok(false);
    }
    save(&entries)?;
    Ok(true)
}

/// Expand a leading `~` to $HOME.
fn expand(entry: &str) -> String {
    if let Some(rest) = entry.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    entry.to_string()
}

fn prefix_match(entry: &str, target: &str) -> bool {
    let entry = entry.trim_end_matches('/');
    target == entry || target.starts_with(&format!("{entry}/"))
}

/// Does any pattern match this repo (by absolute root or scan-relative name)?
pub fn matches(patterns: &[String], name: &str, root: &Path) -> bool {
    let abs = root.to_string_lossy();
    patterns.iter().any(|p| {
        let p = expand(p);
        if p.starts_with('/') {
            prefix_match(&p, &abs)
        } else {
            prefix_match(&p, name)
        }
    })
}

/// All patterns matching this repo, for removal on un-ignore.
pub fn matching_patterns(patterns: &[String], name: &str, root: &Path) -> Vec<String> {
    let abs = root.to_string_lossy().into_owned();
    patterns
        .iter()
        .filter(|p| {
            let e = expand(p);
            if e.starts_with('/') {
                prefix_match(&e, &abs)
            } else {
                prefix_match(&e, name)
            }
        })
        .cloned()
        .collect()
}

/// Canonical entry to store for a path being ignored from the CLI/TUI:
/// the absolute path in `~`-abbreviated form if it exists, else as given.
pub fn entry_for(path: &str) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => display_path(&p),
        Err(_) => path.trim_end_matches('/').to_string(),
    }
}
