//! Shared get-meta / put-meta logic, used by both the CLI and the gRPC server.
//!
//! Metadata is keyed by content hash, so an edit applies to every copy of that
//! content; lookups resolve a path to its hash via the directory manifest.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

use hashit_core::manifest::ManifestCache;
use hashit_core::meta::{self, MetaFile};

/// One path's metadata. Absent fields are omitted from JSON; a lookup failure is
/// reported via `error`.
#[derive(Default, Serialize)]
pub struct MetaView {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Resolve a file to its recorded `(hash, algo, size)` from the manifest in its
/// parent directory. Errors if the file isn't recorded yet.
pub fn entry_for(cache: &mut ManifestCache, path: &Path) -> Result<(String, String, u64)> {
    match cache.entry_for(path) {
        Some(e) => Ok((e.hash, e.algo, e.size)),
        None => bail!(
            "{}: no .hashit entry found; run `hashit scan` on its directory first",
            path.display()
        ),
    }
}

/// Build a `MetaView` for each path (lookup failures captured in `error`).
pub fn gather(meta_folder: &Path, paths: &[impl AsRef<Path>]) -> Result<Vec<MetaView>> {
    let mut cache = ManifestCache::default();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path.as_ref();
        let mut view = MetaView {
            path: path.display().to_string(),
            ..Default::default()
        };
        match entry_for(&mut cache, path) {
            Err(e) => view.error = Some(format!("{e:#}")),
            Ok((hash, algo, size)) => {
                if let Some(m) = MetaFile::load(meta_folder, &hash)? {
                    view.file_type = m.file_type;
                    view.ext = m.ext;
                    view.tags = m.tags;
                    view.properties = m.properties;
                }
                let thumb = meta::thumbnail_path(meta_folder, &hash);
                if thumb.exists() {
                    view.thumbnail = Some(thumb.display().to_string());
                }
                let preview = meta::preview_path(meta_folder, &hash);
                if preview.exists() {
                    view.preview = Some(preview.display().to_string());
                }
                view.hash = Some(hash);
                view.algo = Some(algo);
                view.size = Some(size);
            }
        }
        out.push(view);
    }
    Ok(out)
}

/// Parse `KEY=VALUE` strings, failing on any malformed entry before any write.
pub fn parse_sets(set: &[String]) -> Result<Vec<(String, String)>> {
    set.iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => bail!("invalid --set value (expected KEY=VALUE): {kv}"),
        })
        .collect()
}

/// Apply property `sets`/`removes` to the meta file of each path's content hash.
/// Each unique hash is edited once even if several paths share it. Returns the
/// `(path, hash)` pairs that were updated.
pub fn apply_put(
    meta_folder: &Path,
    paths: &[impl AsRef<Path>],
    sets: &[(String, String)],
    removes: &[String],
) -> Result<Vec<(String, String)>> {
    let mut cache = ManifestCache::default();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut updated = Vec::new();
    for path in paths {
        let path = path.as_ref();
        let (hash, algo, size) = entry_for(&mut cache, path)?;
        if !done.insert(hash.clone()) {
            continue;
        }
        let mut m = MetaFile::load(meta_folder, &hash)?.unwrap_or_default();
        m.hash = hash.clone();
        if m.algo.is_empty() {
            m.algo = algo;
        }
        if m.size == 0 {
            m.size = size;
        }
        for (k, v) in sets {
            m.properties.insert(k.clone(), v.clone());
        }
        for k in removes {
            m.properties.remove(k);
        }
        m.save(meta_folder)?;
        updated.push((path.display().to_string(), hash));
    }
    Ok(updated)
}
