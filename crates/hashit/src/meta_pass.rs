//! Generate per-hash metadata artifacts (thumbnail / preview / EXIF tags) into
//! the shared metadata folder, **once per content hash**.
//!
//! Each artifact is only produced when missing, so re-running a scan with the
//! same `--meta-*` flags is cheap and idempotent. User-authored `properties` in
//! an existing `meta.json` are always preserved.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

use hashit_core::manifest::Manifest;
use hashit_core::meta::{self, MetaFile};
use hashit_core::walk;

/// Which metadata artifacts to ensure exist, and where they live.
pub struct MetaOptions {
    pub folder: PathBuf,
    pub thumbnail: bool,
    pub preview: bool,
    pub tags: bool,
}

#[derive(Default)]
pub struct MetaStats {
    pub hashes: usize,
    pub thumbnails: usize,
    pub previews: usize,
    pub tags: usize,
}

impl std::fmt::Display for MetaStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} hashes, {} thumbnails, {} previews, {} tag sets",
            self.hashes, self.thumbnails, self.previews, self.tags
        )
    }
}

/// A representative on-disk copy of one content hash.
struct Rep {
    path: PathBuf,
    size: u64,
    algo: String,
}

/// First on-disk copy seen for each unique hash across all roots.
fn representatives(roots: &[PathBuf]) -> Result<HashMap<String, Rep>> {
    let mut reps: HashMap<String, Rep> = HashMap::new();
    for root in roots {
        walk::for_each_entry(root, |e| {
            reps.entry(e.entry.hash.clone()).or_insert_with(|| Rep {
                path: e.path,
                size: e.entry.size,
                algo: e.entry.algo,
            });
        })?;
    }
    Ok(reps)
}

/// Flatten an EXIF group+key into a single map key (`group:key`, or just `key`).
fn flatten_key(group: &str, key: &str) -> String {
    if group.is_empty() {
        key.to_string()
    } else {
        format!("{group}:{key}")
    }
}

/// Ensure the requested artifacts exist for one hash; returns which were created.
fn ensure(hash: &str, rep: &Rep, opts: &MetaOptions) -> (bool, bool, bool) {
    let mf = &opts.folder;
    let (mut made_thumb, mut made_preview, mut made_tags) = (false, false, false);

    if opts.thumbnail {
        let dest = meta::thumbnail_path(mf, hash);
        if !dest.exists() {
            made_thumb = hashit_extract::make_thumbnail(&rep.path, &dest);
        }
    }
    if opts.preview {
        let dest = meta::preview_path(mf, hash);
        if !dest.exists() {
            made_preview = hashit_extract::make_preview(&rep.path, &dest);
        }
    }
    if opts.tags {
        let existing = MetaFile::load(mf, hash).ok().flatten();
        // "Missing" tags = no meta.json, or one never run through extraction
        // (extractor_version 0, e.g. created by put-meta). A prior extraction
        // that legitimately found no tags is left alone.
        let needs = existing
            .as_ref()
            .map(|m| m.extractor_version == 0)
            .unwrap_or(true);
        if needs {
            let ex = hashit_extract::extract_tags(&rep.path);
            let mut m = existing.unwrap_or_default();
            m.hash = hash.to_string();
            m.algo = rep.algo.clone();
            m.size = rep.size;
            m.file_type = ex.file_type;
            m.ext = ex.ext;
            m.extractor_version = ex.extractor_version;
            m.tags = ex
                .tags
                .into_iter()
                .map(|t| (flatten_key(&t.group, &t.key), t.value))
                .collect();
            made_tags = m.save(mf).is_ok();
        }
    }
    (made_thumb, made_preview, made_tags)
}

/// Bulk pass over every unique hash under `roots` (parallel across hashes).
pub fn run(roots: &[PathBuf], opts: &MetaOptions, quiet: bool) -> Result<MetaStats> {
    let reps = representatives(roots)?;
    let results: Vec<(bool, bool, bool)> =
        reps.par_iter().map(|(h, rep)| ensure(h, rep, opts)).collect();
    let mut stats = MetaStats {
        hashes: reps.len(),
        ..Default::default()
    };
    for (t, p, g) in results {
        stats.thumbnails += t as usize;
        stats.previews += p as usize;
        stats.tags += g as usize;
    }
    if !quiet {
        println!("meta: {stats}");
    }
    Ok(stats)
}

/// Watch hook: regenerate artifacts for the files in one directory's manifest.
pub fn run_for_dir(dir: &Path, opts: &MetaOptions) -> Result<()> {
    let Some(manifest) = Manifest::load(dir)? else {
        return Ok(());
    };
    let mut seen: HashSet<String> = HashSet::new();
    for (name, entry) in manifest.files {
        if !seen.insert(entry.hash.clone()) {
            continue;
        }
        let rep = Rep {
            path: dir.join(&name),
            size: entry.size,
            algo: entry.algo,
        };
        ensure(&entry.hash, &rep, opts);
    }
    Ok(())
}
