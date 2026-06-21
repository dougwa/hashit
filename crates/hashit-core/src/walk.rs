//! Walk every `.hashit` manifest under a root and yield one record per file.
//!
//! This is the shared traversal behind `hashit`'s metadata pass and
//! `hashit-idx`'s index build: both need to enumerate `(path, hash)` pairs from
//! the manifests without re-hashing anything.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use walkdir::WalkDir;

use crate::manifest::{Manifest, FileEntry, MANIFEST_NAME};

/// One file recorded in some `.hashit` manifest under the walked root.
pub struct WalkEntry {
    /// Absolute (or root-relative, mirroring the input) path to the file itself.
    pub path: PathBuf,
    /// Path relative to the walk root, using forward slashes.
    pub rel: String,
    /// The file's basename.
    pub name: String,
    pub entry: FileEntry,
}

/// Normalize a path to forward-slash form for portable output.
fn to_slash(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        if let Component::Normal(s) = c {
            parts.push(s.to_string_lossy().to_string());
        }
    }
    parts.join("/")
}

/// True if `path`, taken relative to `root`, contains any of the `ignore`
/// substrings. Mirrors `hashit`'s `--ignore` matching (plain substrings, not
/// globs); an empty list never matches.
pub fn is_ignored(path: &Path, root: &Path, ignores: &[String]) -> bool {
    if ignores.is_empty() {
        return false;
    }
    let rel = to_slash(path.strip_prefix(root).unwrap_or(path));
    ignores.iter().any(|s| rel.contains(s.as_str()))
}

/// Invoke `f` for every file listed in every `.hashit` manifest under `root`.
/// Unreadable/invalid manifests are skipped (their error propagates only if the
/// directory entry itself can't be read).
pub fn for_each_entry(root: &Path, f: impl FnMut(WalkEntry)) -> Result<()> {
    for_each_entry_filtered(root, &[], f)
}

/// Like [`for_each_entry`], but prunes any directory whose path (relative to
/// `root`) matches one of the `ignores` substrings, so matching folders and
/// their whole subtrees are never indexed.
pub fn for_each_entry_filtered(
    root: &Path,
    ignores: &[String],
    mut f: impl FnMut(WalkEntry),
) -> Result<()> {
    let walker = WalkDir::new(root)
        .into_iter()
        // Prune ignored directories up front so we never descend into them.
        .filter_entry(|e| !e.file_type().is_dir() || !is_ignored(e.path(), root, ignores));
    for dirent in walker.filter_map(|e| e.ok()) {
        if !dirent.file_type().is_file() || dirent.file_name() != MANIFEST_NAME {
            continue;
        }
        let dir = dirent.path().parent().unwrap_or(root);
        let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new(""));
        let manifest = match Manifest::load(dir) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            // A single unreadable/corrupt `.hashit` shouldn't abort the whole
            // walk: warn and move on so the rest of the tree still indexes.
            Err(e) => {
                eprintln!("warning: skipping manifest in {}: {e:#}", dir.display());
                continue;
            }
        };
        for (name, entry) in manifest.files {
            let mut rel = rel_dir.to_path_buf();
            rel.push(&name);
            f(WalkEntry {
                path: dir.join(&name),
                rel: to_slash(&rel),
                name,
                entry,
            });
        }
    }
    Ok(())
}
