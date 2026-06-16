//! The hashit logical-filesystem API: a typed, transport-agnostic surface over
//! the metadata index. `serve.rs` wraps these in HTTP handlers, but a Rust
//! consumer can call them directly. All operations are read-only.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::dedup::{self, DedupOptions};
use crate::drive;
use crate::extract;
use crate::hash::HashAlgo;
use crate::scan::{self, ScanOptions};
use crate::store::{ContentDetail, DirEntry, DriveRow, QueryFilter, QueryRow, Store, FAVORITE_TAG};

/// Registered drives, with online/offline state refreshed against the disks.
pub fn drives(store: &Store) -> Result<Vec<DriveRow>> {
    drive::refresh_presence(store)?;
    store.list_drives()
}

/// Immediate children of `path` on `drive` ("" = drive root).
pub fn list_dir(store: &Store, drive: &str, path: &str) -> Result<Vec<DirEntry>> {
    store.list_dir(drive, path)
}

/// Describe a single logical path (file or directory), or `None`.
pub fn stat(store: &Store, drive: &str, path: &str) -> Result<Option<DirEntry>> {
    store.stat(drive, path)
}

/// Paginated reverse-index query.
pub fn query(store: &Store, filter: &QueryFilter) -> Result<Vec<QueryRow>> {
    store.query(filter)
}

/// Full detail (metadata, locations, tags, links) for a content hash.
pub fn detail(store: &Store, hash: &str) -> Result<Option<ContentDetail>> {
    store.content_detail(hash)
}

/// Resolve a hash to a readable absolute source path on an online drive.
/// Returns `(algo, hash, abs path)`.
pub fn content_source(store: &Store, hash: &str) -> Result<Option<(String, String, PathBuf)>> {
    store.content_source(hash)
}

/// The thumbnail path for a hash, generating it on demand if missing.
/// `None` if the hash isn't indexed or no thumbnail could be produced.
pub fn thumb(store: &Store, hash: &str) -> Result<Option<PathBuf>> {
    let Some((algo, hash, has_thumb, src)) = store.thumb_lookup(hash)? else {
        return Ok(None);
    };
    let dest = store.thumb_path(&algo, &hash);
    if has_thumb && dest.exists() {
        return Ok(Some(dest));
    }
    if extract::make_thumb(&src, &dest) {
        store.set_has_thumb(&algo, &hash, true)?;
        Ok(Some(dest))
    } else {
        Ok(None)
    }
}

// -- mutations (write API) -------------------------------------------------

/// Add tags to a hash; returns the hash's full tag list afterwards.
pub fn add_tags(store: &Store, hash: &str, tags: &[String]) -> Result<Vec<String>> {
    let (algo, h) = store.resolve_hash(hash)?;
    for t in tags {
        store.add_tag(&algo, &h, t)?;
    }
    store.list_tags(&algo, &h)
}

/// Remove a tag from a hash; returns the remaining tags.
pub fn remove_tag(store: &Store, hash: &str, tag: &str) -> Result<Vec<String>> {
    let (algo, h) = store.resolve_hash(hash)?;
    store.remove_tag(&algo, &h, tag)?;
    store.list_tags(&algo, &h)
}

/// Set or clear the favorite mark on a hash.
pub fn set_favorite(store: &Store, hash: &str, on: bool) -> Result<()> {
    let (algo, h) = store.resolve_hash(hash)?;
    if on {
        store.add_tag(&algo, &h, FAVORITE_TAG)?;
    } else {
        store.remove_tag(&algo, &h, FAVORITE_TAG)?;
    }
    Ok(())
}

/// Link a set of hashes into one group; returns the group id.
pub fn link(store: &mut Store, hashes: &[String]) -> Result<String> {
    if hashes.len() < 2 {
        anyhow::bail!("link requires at least two hashes");
    }
    let mut members = Vec::with_capacity(hashes.len());
    for h in hashes {
        members.push(store.resolve_hash(h)?);
    }
    store.link_hashes(&members)
}

/// Remove a hash from its link group.
pub fn unlink(store: &mut Store, hash: &str) -> Result<bool> {
    let (algo, h) = store.resolve_hash(hash)?;
    store.unlink_hash(&algo, &h)
}

/// The outcome of a dedup "keep this" operation.
#[derive(Debug, Serialize)]
pub struct DedupOutcome {
    /// `drive_id:path` of the copy that was kept.
    pub kept: String,
    /// `drive_id:path` of each removed copy.
    pub removed: Vec<String>,
    /// Copies that couldn't be removed because their drive is offline.
    pub skipped_offline: usize,
}

/// Keep one copy of a content hash and delete the other on-disk copies, leaving
/// a `<file>.dedup` pointer to the kept file (matching the `dedup` CLI). The
/// affected manifests and the index are reconciled. Offline copies are skipped.
pub fn dedup_keep(
    store: &mut Store,
    hash: &str,
    keep_drive: &str,
    keep_path: &str,
) -> Result<DedupOutcome> {
    let (algo, h) = store.resolve_hash(hash)?;
    let locs = store.locations_for(&algo, &h)?;

    let keep = locs
        .iter()
        .find(|l| l.drive_id == keep_drive && l.path == keep_path)
        .ok_or_else(|| anyhow::anyhow!("keep location {keep_drive}:{keep_path} not found"))?;
    if !keep.online {
        anyhow::bail!("the drive holding the kept copy is offline");
    }
    let keep_abs = Path::new(&keep.last_root).join(&keep.path);

    let dd = DedupOptions {
        interactive: false,
        write_links: true,
        dry_run: false,
    };
    let opts = dedup_scan_opts(&algo);

    let mut removed = Vec::new();
    let mut skipped_offline = 0usize;
    // (drive_root, dir) pairs to reconcile after deletions.
    let mut dirs: BTreeSet<(String, PathBuf)> = BTreeSet::new();

    for l in &locs {
        if l.drive_id == keep_drive && l.path == keep_path {
            continue;
        }
        if !l.online {
            skipped_offline += 1;
            continue;
        }
        let abs = Path::new(&l.last_root).join(&l.path);
        dedup::remove_one(&abs, &keep_abs, &dd)?;
        store.remove_location(&algo, &h, &l.drive_id, &l.path)?;
        removed.push(format!("{}:{}", l.drive_id, l.path));
        if let Some(dir) = abs.parent() {
            dirs.insert((l.last_root.clone(), dir.to_path_buf()));
        }
    }

    // Reconcile each affected directory's manifest (drops removed entries).
    for (root, dir) in &dirs {
        let _ = scan::process_dir(dir, &opts, Path::new(root));
    }

    Ok(DedupOutcome {
        kept: format!("{keep_drive}:{keep_path}"),
        removed,
        skipped_offline,
    })
}

/// Minimal scan options for reconciling a directory after a dedup deletion.
/// The algo matches the deduped hash so remaining files aren't needlessly
/// rehashed.
fn dedup_scan_opts(algo: &str) -> ScanOptions {
    ScanOptions {
        algo: if algo == HashAlgo::Sha256.name() {
            HashAlgo::Sha256
        } else {
            HashAlgo::Blake3
        },
        follow_symlinks: false,
        excludes: Vec::new(),
        ignores: Vec::new(),
        quiet: true,
        verbose: false,
        status: false,
        skip_apple_double: true,
        dry_run: false,
    }
}
