//! The hashit logical-filesystem API: a typed, transport-agnostic surface over
//! the metadata index. `serve.rs` wraps these in HTTP handlers, but a Rust
//! consumer can call them directly. All operations are read-only.

use std::path::PathBuf;

use anyhow::Result;

use crate::drive;
use crate::extract;
use crate::store::{ContentDetail, DirEntry, DriveRow, QueryFilter, QueryRow, Store};

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
