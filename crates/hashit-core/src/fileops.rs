use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

use crate::manifest::{is_manifest_file, mtime_ns, set_mtime, FileEntry, Manifest, ManifestCache};
use crate::scan::{is_apple_double, is_dedup_pointer, process_dir, scan, ScanOptions};

pub struct OpOptions {
    pub recursive: bool,
    pub force: bool,
    pub quiet: bool,
}

/// Mutable state threaded through an operation: manifests to seed, directories
/// to reconcile, and a cache of source manifests.
#[derive(Default)]
struct State {
    affected: BTreeSet<PathBuf>,
    staged: BTreeMap<PathBuf, Vec<(String, FileEntry)>>,
    cache: ManifestCache,
}

/// Directory that contains `p`. A bare relative name like `foo.DNG` has a
/// `parent()` of `Some("")`, not `None`; treat that (and any parent-less path)
/// as the current directory so scans and manifest updates hit the right dir.
fn parent_dir(p: &Path) -> PathBuf {
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Scan each source first so its `.hashit` is current before the operation:
/// directories are scanned recursively, individual files have their containing
/// directory refreshed. Deduplicated so the same location isn't scanned twice.
fn scan_sources(sources: &[PathBuf], scan_opts: &ScanOptions) -> Result<()> {
    let mut scanned: BTreeSet<PathBuf> = BTreeSet::new();
    for src in sources {
        if src.is_dir() {
            let root = src.canonicalize().unwrap_or_else(|_| src.clone());
            if scanned.insert(root.clone()) {
                scan(&root, scan_opts)?;
            }
        } else if src.is_file() {
            let parent = parent_dir(src);
            let dir = parent.canonicalize().unwrap_or(parent);
            if scanned.insert(dir.clone()) {
                process_dir(&dir, scan_opts, &dir)?;
            }
        }
    }
    Ok(())
}

/// Carry the source file's manifest entry to the target, keeping its hash but
/// re-reading size/mtime so the entry stays consistent with the stored file.
fn stage_from_source(src: &Path, target: &Path, st: &mut State) {
    if let Some(mut e) = st.cache.entry_for(src) {
        set_mtime(target, e.mtime_ns);
        if let Ok(md) = fs::metadata(target) {
            e.size = md.len();
            e.mtime_ns = mtime_ns(&md);
        }
        if let Some(name) = target.file_name() {
            st.staged
                .entry(parent_dir(target))
                .or_default()
                .push((name.to_string_lossy().to_string(), e));
        }
    }
}

fn copy_file(src: &Path, target: &Path, opts: &OpOptions, st: &mut State) -> Result<()> {
    if target.exists() && !opts.force {
        bail!("{} already exists (use -f to overwrite)", target.display());
    }
    let parent = parent_dir(target);
    fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    st.affected.insert(parent);
    fs::copy(src, target)
        .with_context(|| format!("copying {} -> {}", src.display(), target.display()))?;
    stage_from_source(src, target, st);
    Ok(())
}

/// Recursively copy `src_dir` into `target_base`, skipping hashit metadata and
/// symlinks, carrying each file's manifest entry.
fn copy_tree(src_dir: &Path, target_base: &Path, opts: &OpOptions, st: &mut State) -> Result<()> {
    for entry in WalkDir::new(src_dir).into_iter().filter_map(|e| e.ok()) {
        let rel = match entry.path().strip_prefix(src_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let target = target_base.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
        } else if ft.is_file() {
            let name = entry.file_name().to_string_lossy();
            if is_manifest_file(&name) || is_dedup_pointer(&name) || is_apple_double(&name) {
                continue;
            }
            copy_file(entry.path(), &target, opts, st)?;
        }
    }
    Ok(())
}

/// Resolve the destination for a single source: into `dest` if it's a directory,
/// otherwise `dest` itself.
fn target_for(dest: &Path, src: &Path, dest_is_dir: bool) -> Result<PathBuf> {
    if dest_is_dir {
        let base = src
            .file_name()
            .with_context(|| format!("invalid source path {}", src.display()))?;
        Ok(dest.join(base))
    } else {
        Ok(dest.to_path_buf())
    }
}

/// Seed carried manifest entries, then reconcile every affected directory so
/// each `.hashit` matches its directory (dropping removed files, hashing any
/// file that had no source entry, reusing carried hashes).
fn finalize(st: &State, scan_opts: &ScanOptions) {
    for (dir, entries) in &st.staged {
        let mut files = Manifest::load(dir)
            .ok()
            .flatten()
            .map(|m| m.files)
            .unwrap_or_default();
        for (name, entry) in entries {
            files.insert(name.clone(), entry.clone());
        }
        let _ = Manifest::new(files).save(dir);
    }
    let mut refresh = scan_opts.clone();
    refresh.quiet = true;
    refresh.verbose = false;
    refresh.status = false;
    for dir in &st.affected {
        let _ = process_dir(dir, &refresh, dir);
    }
}

pub fn cp(paths: &[PathBuf], opts: &OpOptions, scan_opts: &ScanOptions) -> Result<()> {
    let (dest, sources) = paths
        .split_last()
        .context("cp requires one or more sources and a destination")?;
    let dest_is_dir = dest.is_dir();
    if sources.len() > 1 && !dest_is_dir {
        bail!("target '{}' is not a directory", dest.display());
    }
    scan_sources(sources, scan_opts)?;
    let mut st = State::default();
    for src in sources {
        if !src.exists() {
            bail!("{}: no such file or directory", src.display());
        }
        let target = target_for(dest, src, dest_is_dir)?;
        if src.is_dir() {
            if !opts.recursive {
                bail!("-r not specified; omitting directory '{}'", src.display());
            }
            copy_tree(src, &target, opts, &mut st)?;
            if !opts.quiet {
                println!("copied {} -> {} (recursive)", src.display(), target.display());
            }
        } else {
            copy_file(src, &target, opts, &mut st)?;
            if !opts.quiet {
                println!("copied {} -> {}", src.display(), target.display());
            }
        }
    }
    finalize(&st, scan_opts);
    Ok(())
}

fn move_one(src: &Path, target: &Path, opts: &OpOptions, st: &mut State) -> Result<()> {
    if src == target {
        return Ok(());
    }
    if target.exists() && !opts.force {
        bail!("{} already exists (use -f to overwrite)", target.display());
    }
    let is_dir = src.is_dir();
    // Capture the source file entry before the move (the source dir manifest
    // still has it even after a rename, but grabbing it now is simplest).
    let src_entry = if is_dir { None } else { st.cache.entry_for(src) };

    let parent = parent_dir(target);
    fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    if target.exists() && opts.force {
        if target.is_dir() {
            fs::remove_dir_all(target)
                .with_context(|| format!("removing {}", target.display()))?;
        } else {
            fs::remove_file(target).with_context(|| format!("removing {}", target.display()))?;
        }
    }

    // Try a rename first; fall back to copy+remove across filesystems.
    if fs::rename(src, target).is_err() {
        if is_dir {
            copy_tree(src, target, opts, st)?;
            fs::remove_dir_all(src).with_context(|| format!("removing {}", src.display()))?;
        } else {
            fs::copy(src, target)
                .with_context(|| format!("copying {} -> {}", src.display(), target.display()))?;
            fs::remove_file(src).with_context(|| format!("removing {}", src.display()))?;
        }
    }

    if is_dir {
        // A renamed directory carries its .hashit files with it; parent
        // manifests list only files, so nothing to update there. The fallback
        // path staged target entries via copy_tree.
    } else {
        st.affected.insert(parent_dir(src));
        if let Some(mut e) = src_entry {
            set_mtime(target, e.mtime_ns);
            if let Ok(md) = fs::metadata(target) {
                e.size = md.len();
                e.mtime_ns = mtime_ns(&md);
            }
            if let Some(name) = target.file_name() {
                st.staged
                    .entry(parent_dir(target))
                    .or_default()
                    .push((name.to_string_lossy().to_string(), e));
            }
        }
        st.affected.insert(parent_dir(target));
    }

    if !opts.quiet {
        println!("moved {} -> {}", src.display(), target.display());
    }
    Ok(())
}

pub fn mv(paths: &[PathBuf], opts: &OpOptions, scan_opts: &ScanOptions) -> Result<()> {
    let (dest, sources) = paths
        .split_last()
        .context("mv requires one or more sources and a destination")?;
    let dest_is_dir = dest.is_dir();
    if sources.len() > 1 && !dest_is_dir {
        bail!("target '{}' is not a directory", dest.display());
    }
    scan_sources(sources, scan_opts)?;
    let mut st = State::default();
    for src in sources {
        if !src.exists() {
            bail!("{}: no such file or directory", src.display());
        }
        let target = target_for(dest, src, dest_is_dir)?;
        move_one(src, &target, opts, &mut st)?;
    }
    finalize(&st, scan_opts);
    Ok(())
}

pub fn rm(paths: &[PathBuf], opts: &OpOptions, scan_opts: &ScanOptions) -> Result<()> {
    scan_sources(paths, scan_opts)?;
    let mut st = State::default();
    for p in paths {
        if !p.exists() {
            if opts.force {
                continue;
            }
            bail!("{}: no such file or directory", p.display());
        }
        if p.is_dir() {
            if !opts.recursive {
                bail!("{} is a directory (use -r)", p.display());
            }
            fs::remove_dir_all(p).with_context(|| format!("removing {}", p.display()))?;
            if !opts.quiet {
                println!("removed {} (recursive)", p.display());
            }
            // Parent manifests list only files, so no manifest update is needed.
        } else {
            fs::remove_file(p).with_context(|| format!("removing {}", p.display()))?;
            st.affected.insert(parent_dir(p));
            if !opts.quiet {
                println!("removed {}", p.display());
            }
        }
    }
    finalize(&st, scan_opts);
    Ok(())
}
