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

/// Invoke `f` for every file listed in every `.hashit` manifest under `root`.
/// Unreadable/invalid manifests are skipped (their error propagates only if the
/// directory entry itself can't be read).
pub fn for_each_entry(root: &Path, mut f: impl FnMut(WalkEntry)) -> Result<()> {
    for dirent in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !dirent.file_type().is_file() || dirent.file_name() != MANIFEST_NAME {
            continue;
        }
        let dir = dirent.path().parent().unwrap_or(root);
        let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new(""));
        let Some(manifest) = Manifest::load(dir)? else {
            continue;
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
