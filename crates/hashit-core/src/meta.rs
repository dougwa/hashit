//! Per-content metadata stored as plain files, keyed by content hash.
//!
//! Each hash gets up to three sibling artifacts in a single, shared metadata
//! folder (`--meta-folder`, default `~/.hashit-meta`), sharded by hash prefix so
//! one directory never holds millions of entries:
//!
//! ```text
//! <meta-folder>/1a/fa/1afaef….meta.json     structured metadata (this module)
//! <meta-folder>/1a/fa/1afaef….thumbnail.jpg  512px thumbnail (hashit-extract)
//! <meta-folder>/1a/fa/1afaef….preview.jpg    2048px preview  (hashit-extract)
//! ```
//!
//! The artifacts are derived data: everything here is rebuildable by re-running
//! `hashit scan --meta-*`, except user-authored `properties`, which are the only
//! state that can't be recovered from the original files.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::manifest::now_rfc3339;

pub const META_SUFFIX: &str = "meta.json";
pub const THUMBNAIL_SUFFIX: &str = "thumbnail.jpg";
pub const PREVIEW_SUFFIX: &str = "preview.jpg";

/// Resolve the metadata folder to use: an explicit override, else `$HASHIT_META`,
/// else `~/.hashit-meta` (falling back to `./.hashit-meta` without a home dir).
pub fn resolve_meta_folder(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    if let Ok(p) = std::env::var("HASHIT_META") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".hashit-meta");
    }
    PathBuf::from(".hashit-meta")
}

/// The two shard components for a hash: its first two and next two hex chars.
fn shard(hash: &str) -> (&str, &str) {
    let a = &hash[..hash.len().min(2)];
    let b = if hash.len() >= 4 { &hash[2..4] } else { "" };
    (a, b)
}

/// Path to one artifact (`suffix` = `meta.json` / `thumbnail.jpg` / …) for `hash`.
pub fn artifact_path(meta_folder: &Path, hash: &str, suffix: &str) -> PathBuf {
    let (a, b) = shard(hash);
    meta_folder
        .join(a)
        .join(b)
        .join(format!("{hash}.{suffix}"))
}

pub fn meta_path(meta_folder: &Path, hash: &str) -> PathBuf {
    artifact_path(meta_folder, hash, META_SUFFIX)
}

pub fn thumbnail_path(meta_folder: &Path, hash: &str) -> PathBuf {
    artifact_path(meta_folder, hash, THUMBNAIL_SUFFIX)
}

pub fn preview_path(meta_folder: &Path, hash: &str) -> PathBuf {
    artifact_path(meta_folder, hash, PREVIEW_SUFFIX)
}

/// Structured per-hash metadata: extracted EXIF `tags` plus user-authored
/// `properties`. Thumbnail/preview presence is *not* recorded here — it's a plain
/// file-existence check at the deterministic path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetaFile {
    pub hash: String,
    pub algo: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext: Option<String>,
    #[serde(default)]
    pub extractor_version: i64,
    /// Extracted EXIF tags, keyed `group:key` to avoid cross-group collisions.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// User-authored properties set via `hashit put-meta`.
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    pub updated_at: String,
}

impl MetaFile {
    /// Load `<meta-folder>/…/<hash>.meta.json`, or `Ok(None)` if absent.
    pub fn load(meta_folder: &Path, hash: &str) -> Result<Option<MetaFile>> {
        let path = meta_path(meta_folder, hash);
        match fs::read(&path) {
            Ok(bytes) => {
                let m = serde_json::from_slice::<MetaFile>(&bytes)
                    .with_context(|| format!("parsing meta {}", path.display()))?;
                Ok(Some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading meta {}", path.display())),
        }
    }

    /// Write the meta file atomically (temp file + rename), refreshing
    /// `updated_at` and creating shard directories as needed.
    pub fn save(&mut self, meta_folder: &Path) -> Result<()> {
        self.updated_at = now_rfc3339();
        let path = meta_path(meta_folder, &self.hash);
        let dir = path
            .parent()
            .expect("meta path always has a shard parent directory");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = dir.join(format!(
            ".{}.tmp.{}.{:?}",
            self.hash,
            std::process::id(),
            std::thread::current().id()
        ));
        let json = serde_json::to_vec_pretty(self).context("serializing meta")?;
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("flushing {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }
}
