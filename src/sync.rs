use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;

use crate::diff::{collect, HashIndex};
use crate::manifest::{mtime_ns, set_mtime, FileEntry, Manifest, ManifestCache};
use crate::scan::{process_dir, scan, ScanOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Direction {
    /// Copy files present only in path1 into path2 (default).
    To,
    /// Copy files present only in path2 into path1.
    From,
    /// Copy missing files in both directions.
    Both,
}

pub struct SyncOptions {
    pub direction: Direction,
    /// Scan both paths before comparing (refreshes their .hashit manifests).
    pub scan_first: bool,
    pub dry_run: bool,
}

/// One side of a sync: a root path and its content-hash index.
struct Side<'a> {
    root: &'a Path,
    idx: &'a HashIndex,
}

/// Mutable accumulators shared across copy passes.
#[derive(Default)]
struct Accum {
    copied: usize,
    renamed: usize,
    /// Target directories that received files (need manifest reconciliation).
    affected: BTreeSet<PathBuf>,
    /// Target dir -> (destination name, carried-over manifest entry).
    staged: BTreeMap<PathBuf, Vec<(String, FileEntry)>>,
    /// Cache of source dir manifests, loaded lazily.
    src_cache: ManifestCache,
}

/// Pick a destination path under `dst_root` for `rel`, appending `_N` to the
/// file name until it doesn't collide with an existing file.
fn unique_dest(dst_root: &Path, rel: &str) -> PathBuf {
    let initial = dst_root.join(rel);
    if !initial.exists() {
        return initial;
    }
    let parent = initial.parent().unwrap_or(dst_root).to_path_buf();
    let stem = initial
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = initial.extension().map(|e| e.to_string_lossy().to_string());
    for n in 1u64.. {
        let name = match &ext {
            Some(e) => format!("{stem}_{n}.{e}"),
            None => format!("{stem}_{n}"),
        };
        let cand = parent.join(name);
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!("exhausted suffixes")
}

/// Copy every file whose hash is absent from `dst` from `src` into `dst` at its
/// source-relative path (renaming on collision), carrying over its manifest
/// entry so the copied content isn't re-hashed.
fn copy_missing(
    label: &str,
    src: &Side,
    dst: &Side,
    so: &SyncOptions,
    quiet: bool,
    accum: &mut Accum,
) -> Result<()> {
    if !quiet {
        println!("{label}:");
    }
    for (key, files) in src.idx {
        if dst.idx.contains_key(key) {
            continue; // content already present in target
        }
        for rel in files {
            let src_abs = src.root.join(rel);
            let dest = unique_dest(dst.root, rel);
            let dst_rel = dest.strip_prefix(dst.root).unwrap_or(&dest).to_path_buf();
            let renamed = dst_rel.to_string_lossy() != *rel;

            if !so.dry_run {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                    accum.affected.insert(parent.to_path_buf());
                }
                fs::copy(&src_abs, &dest).with_context(|| {
                    format!("copying {} -> {}", src_abs.display(), dest.display())
                })?;

                // Carry the source's .hashit entry into the target manifest,
                // keeping the hash but re-reading size/mtime so the entry stays
                // consistent with the file as actually stored.
                if let Some(mut entry) = accum.src_cache.entry_for(&src_abs) {
                    set_mtime(&dest, entry.mtime_ns);
                    if let Ok(md) = fs::metadata(&dest) {
                        entry.size = md.len();
                        entry.mtime_ns = mtime_ns(&md);
                    }
                    let dest_dir = dest.parent().unwrap_or(dst.root).to_path_buf();
                    let dest_name = dest
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    accum.staged.entry(dest_dir).or_default().push((dest_name, entry));
                }
            }

            accum.copied += 1;
            if renamed {
                accum.renamed += 1;
            }
            if !quiet {
                let verb = if so.dry_run { "would copy" } else { "copy" };
                let note = if renamed { "  (renamed)" } else { "" };
                println!("  {verb} {rel}  =>  {}{note}", dst_rel.to_string_lossy());
            }
        }
    }
    Ok(())
}

pub fn sync(p1: &Path, p2: &Path, scan_opts: &ScanOptions, so: &SyncOptions) -> Result<()> {
    let r1 = p1
        .canonicalize()
        .with_context(|| format!("resolving {}", p1.display()))?;
    let r2 = p2
        .canonicalize()
        .with_context(|| format!("resolving {}", p2.display()))?;

    if so.scan_first {
        let s1 = scan(&r1, scan_opts)?;
        let s2 = scan(&r2, scan_opts)?;
        if !scan_opts.quiet {
            eprintln!("scan {}: {s1}", r1.display());
            eprintln!("scan {}: {s2}", r2.display());
        }
    }

    // Index both sides once, up front, so copies don't perturb the comparison.
    let idx1 = collect(&r1, scan_opts)?;
    let idx2 = collect(&r2, scan_opts)?;

    let q = scan_opts.quiet;
    let s1 = Side { root: &r1, idx: &idx1 };
    let s2 = Side { root: &r2, idx: &idx2 };
    let mut accum = Accum::default();
    match so.direction {
        Direction::To => copy_missing("path1 -> path2", &s1, &s2, so, q, &mut accum)?,
        Direction::From => copy_missing("path2 -> path1", &s2, &s1, so, q, &mut accum)?,
        Direction::Both => {
            copy_missing("path1 -> path2", &s1, &s2, so, q, &mut accum)?;
            copy_missing("path2 -> path1", &s2, &s1, so, q, &mut accum)?;
        }
    }

    if !so.dry_run {
        // Seed target manifests with the carried-over entries.
        for (dir, entries) in &accum.staged {
            let mut files = Manifest::load(dir)
                .ok()
                .flatten()
                .map(|m| m.files)
                .unwrap_or_default();
            for (name, entry) in entries {
                files.insert(name.clone(), entry.clone());
            }
            if let Err(e) = Manifest::new(files).save(dir) {
                if !q {
                    eprintln!("hashit: error writing manifest {}: {e:#}", dir.display());
                }
            }
        }
        // Reconcile affected dirs. Seeded entries are reused (size+mtime match),
        // so this only hashes files that lacked a source entry, and fixes up
        // anything else — using whichever root each directory lives under.
        let mut refresh = scan_opts.clone();
        refresh.quiet = true;
        refresh.verbose = false;
        refresh.status = false;
        for d in &accum.affected {
            let root = if d.starts_with(&r2) { &r2 } else { &r1 };
            let _ = process_dir(d, &refresh, root);
        }
    }

    let prefix = if so.dry_run { "dry-run: " } else { "" };
    println!(
        "{prefix}copied {} files ({} renamed to avoid collisions)",
        accum.copied, accum.renamed
    );
    Ok(())
}
