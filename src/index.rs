//! Orchestrates the global index: scan → reconcile store → extract per new hash.
//!
//! This is the M1 workhorse behind `hashit index`. It deliberately keeps the
//! core `scan` path untouched: it reuses `inventory::build_inventory` to read
//! the freshly-written `.hashit` manifests, then upserts the global store.
//! Extraction runs **once per content hash** — duplicates only add a location.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;

use crate::drive;
use crate::extract;
use crate::inventory::build_inventory;
use crate::manifest::{now_rfc3339, Manifest};
use crate::scan::{self, ScanOptions};
use crate::store::{ContentMeta, Store};

#[derive(Default, Debug)]
pub struct IndexStats {
    pub files: usize,
    pub new_content: usize,
    pub thumbs: usize,
}

impl IndexStats {
    fn merge(&mut self, o: IndexStats) {
        self.files += o.files;
        self.new_content += o.new_content;
        self.thumbs += o.thumbs;
    }
}

/// Index one or more roots into the global store.
pub fn index(
    roots: &[PathBuf],
    opts: &ScanOptions,
    no_scan: bool,
    reindex: bool,
) -> Result<IndexStats> {
    let mut store = Store::open_default()?;
    let mut total = IndexStats::default();
    for root in roots {
        if !no_scan {
            scan::scan(root, opts)?;
        }
        total.merge(index_root(&mut store, root, opts, reindex)?);
    }
    // Reconcile online/offline across every known drive (some may be unplugged).
    drive::refresh_presence(&store)?;
    Ok(total)
}

fn index_root(
    store: &mut Store,
    root: &Path,
    opts: &ScanOptions,
    reindex: bool,
) -> Result<IndexStats> {
    let marker = drive::load_or_create(root)?;
    store.upsert_drive(&marker.drive_id, &marker.label, root)?;

    // A single timestamp for the whole run: locations refreshed now survive the
    // stale-prune below; anything older (vanished files) is dropped.
    let run_at = now_rfc3339();
    let records = build_inventory(root, opts)?;

    // Collect the unique content hashes that still need extraction, mapped to a
    // representative absolute path. Skip hashes already in the store unless
    // reindexing.
    let mut targets: HashMap<(String, String), (PathBuf, u64)> = HashMap::new();
    for r in &records {
        let key = (r.algo.clone(), r.hash.clone());
        if targets.contains_key(&key) {
            continue;
        }
        if !reindex && store.has_content(&r.algo, &r.hash)? {
            continue;
        }
        targets.insert(key, (root.join(&r.path), r.size));
    }

    // Extract in parallel (file read + EXIF + thumbnail); the store is not
    // touched here — thumbnail destinations are precomputed so the closures
    // need no store access.
    let jobs: Vec<(String, String, PathBuf, u64, PathBuf)> = targets
        .into_iter()
        .map(|((algo, hash), (abs, size))| {
            let dest = store.thumb_path(&algo, &hash);
            (algo, hash, abs, size, dest)
        })
        .collect();
    let extracted: Vec<(String, String, ContentMeta)> = jobs
        .par_iter()
        .map(|(algo, hash, abs, size, dest)| {
            let meta = extract::extract_all(abs, *size, dest);
            (algo.clone(), hash.clone(), meta)
        })
        .collect();

    let mut stats = IndexStats {
        files: records.len(),
        ..Default::default()
    };
    // Serialize the DB writes.
    for (algo, hash, meta) in &extracted {
        if reindex {
            store.forget_content(algo, hash)?;
        }
        if meta.has_thumb {
            stats.thumbs += 1;
        }
        store.insert_content(algo, hash, meta)?;
        stats.new_content += 1;
    }
    // Record where every file lives (new content and previously-known alike).
    for r in &records {
        store.upsert_location(
            &r.algo,
            &r.hash,
            &marker.drive_id,
            &r.path,
            r.mtime_ns,
            &run_at,
        )?;
    }
    // Drop locations on this drive that weren't refreshed (files that vanished).
    store.prune_stale_locations(&marker.drive_id, &run_at)?;
    Ok(stats)
}

/// Detect sidecar groups under `paths`: files in the same directory that share a
/// basename (stem) but differ in extension — e.g. `IMG_0001.JPG` + `IMG_0001.CR2`.
/// Returns each group's distinct `(algo, hash)` members (only groups of 2+).
pub fn auto_link_groups(paths: &[PathBuf]) -> Result<Vec<Vec<(String, String)>>> {
    use std::collections::{BTreeMap, HashSet};
    // Minimal scan options: we only read existing manifests.
    let opts = ScanOptions {
        algo: crate::hash::HashAlgo::Blake3,
        follow_symlinks: false,
        excludes: Vec::new(),
        ignores: Vec::new(),
        quiet: true,
        verbose: false,
        status: false,
        skip_apple_double: true,
        dry_run: false,
    };
    // (dir, lowercased stem) -> [(ext, algo, hash)]
    type StemEntry = (String, String, String);
    let mut by_stem: BTreeMap<(String, String), Vec<StemEntry>> = BTreeMap::new();
    for p in paths {
        for r in build_inventory(p, &opts)? {
            let path = Path::new(&r.path);
            let dir = path
                .parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            by_stem
                .entry((dir, stem))
                .or_default()
                .push((ext, r.algo, r.hash));
        }
    }
    let mut groups = Vec::new();
    for (_, members) in by_stem {
        let exts: HashSet<&String> = members.iter().map(|(e, _, _)| e).collect();
        // Need at least two files differing in extension (a JPG + RAW pair),
        // not just duplicate copies of one file.
        if members.len() < 2 || exts.len() < 2 {
            continue;
        }
        let mut seen = HashSet::new();
        let mut hs = Vec::new();
        for (_, algo, hash) in members {
            if seen.insert((algo.clone(), hash.clone())) {
                hs.push((algo, hash));
            }
        }
        if hs.len() >= 2 {
            groups.push(hs);
        }
    }
    Ok(groups)
}

/// Forward-slash relative path, matching the form used in `locations.path`.
fn to_slash(p: &Path) -> String {
    use std::path::Component;
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// One file to reconcile into the store.
struct FileRef<'a> {
    algo: &'a str,
    hash: &'a str,
    /// Forward-slash path relative to the drive root.
    rel: &'a str,
    /// Absolute path on disk (for extraction).
    abs: &'a Path,
    size: u64,
    mtime_ns: u64,
}

/// Extract a hash's metadata if not already known, then record its location.
fn ingest(store: &mut Store, drive_id: &str, run_at: &str, f: &FileRef) -> Result<()> {
    if !store.has_content(f.algo, f.hash)? {
        let dest = store.thumb_path(f.algo, f.hash);
        let meta = extract::extract_all(f.abs, f.size, &dest);
        store.insert_content(f.algo, f.hash, &meta)?;
    }
    store.upsert_location(f.algo, f.hash, drive_id, f.rel, f.mtime_ns, run_at)?;
    Ok(())
}

/// A long-lived indexer that reconciles individual directories as they change,
/// reusing one open store and caching each root's drive id. Used by `watch`.
pub struct Indexer {
    store: Store,
    drives: std::collections::HashMap<PathBuf, String>,
}

impl Indexer {
    pub fn new() -> Result<Indexer> {
        Ok(Indexer {
            store: Store::open_default()?,
            drives: HashMap::new(),
        })
    }

    fn drive_for(&mut self, root: &Path) -> Result<String> {
        if let Some(id) = self.drives.get(root) {
            return Ok(id.clone());
        }
        let m = drive::load_or_create(root)?;
        self.store.upsert_drive(&m.drive_id, &m.label, root)?;
        self.drives.insert(root.to_path_buf(), m.drive_id.clone());
        Ok(m.drive_id)
    }

    /// Reconcile the freshly-rewritten manifest of `dir` (under `root`) into the
    /// store: new content is extracted, every entry's location upserted.
    ///
    /// Watch deltas are small, so this is sequential. It does not prune store
    /// locations for files removed from `dir` — a full `hashit index` run does
    /// that; here a vanished file simply leaves a stale location until then.
    pub fn reconcile_dir(&mut self, root: &Path, dir: &Path, opts: &ScanOptions) -> Result<()> {
        let drive_id = self.drive_for(root)?;
        let run_at = now_rfc3339();
        let manifest = match Manifest::load(dir)? {
            Some(m) => m,
            None => return Ok(()),
        };
        let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new(""));
        for (name, fe) in manifest.files {
            let abs = dir.join(&name);
            if opts.is_skipped(&abs, root) {
                continue;
            }
            let mut rel = rel_dir.to_path_buf();
            rel.push(&name);
            ingest(
                &mut self.store,
                &drive_id,
                &run_at,
                &FileRef {
                    algo: &fe.algo,
                    hash: &fe.hash,
                    rel: &to_slash(&rel),
                    abs: &abs,
                    size: fe.size,
                    mtime_ns: fe.mtime_ns,
                },
            )?;
        }
        Ok(())
    }
}
