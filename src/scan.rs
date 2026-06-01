use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use glob::Pattern;
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::hash::{hash_file, HashAlgo};
use crate::manifest::{is_manifest_file, now_rfc3339, FileEntry, Manifest};

#[derive(Clone)]
pub struct ScanOptions {
    pub algo: HashAlgo,
    pub follow_symlinks: bool,
    pub excludes: Vec<Pattern>,
    pub quiet: bool,
    pub verbose: bool,
    /// Print a status line (new/modified/unchanged/removed) for every file.
    pub status: bool,
    /// Skip macOS AppleDouble (`._*`) sidecar files.
    pub skip_apple_double: bool,
    pub dry_run: bool,
}

/// True if `name` is a macOS AppleDouble sidecar (e.g. `._.hashit`, `._photo.jpg`).
/// These hold a file's xattrs/resource fork on filesystems (exFAT, FAT, SMB)
/// that can't store them natively; they are OS metadata, not real content.
pub fn is_apple_double(name: &str) -> bool {
    name.starts_with("._")
}

/// Suffix of the pointer files left behind by dedup for removed duplicates.
pub const DEDUP_SUFFIX: &str = ".dedup";

/// True if `name` is a dedup pointer file. These are hashit-managed metadata
/// and must never be hashed or inventoried.
pub fn is_dedup_pointer(name: &str) -> bool {
    name.ends_with(DEDUP_SUFFIX)
}

/// Emit one aligned status line; `println!` locks stdout so lines stay intact
/// even when directories are processed in parallel.
fn print_status(status: &str, path: &Path) {
    println!("{status:9} {}", path.display());
}

impl ScanOptions {
    /// True if `path` matches any exclude pattern (against basename or path
    /// relative to `root`).
    pub fn is_excluded(&self, path: &Path, root: &Path) -> bool {
        if self.excludes.is_empty() {
            return false;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        self.excludes
            .iter()
            .any(|p| p.matches(&name) || p.matches(&rel))
    }
}

#[derive(Default, Debug, Clone)]
pub struct ScanStats {
    pub new: usize,
    pub modified: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub dirs_scanned: usize,
    pub dirs_written: usize,
    pub bytes_hashed: u64,
    pub errors: usize,
}

impl ScanStats {
    pub fn merge(mut self, other: ScanStats) -> ScanStats {
        self.new += other.new;
        self.modified += other.modified;
        self.unchanged += other.unchanged;
        self.removed += other.removed;
        self.dirs_scanned += other.dirs_scanned;
        self.dirs_written += other.dirs_written;
        self.bytes_hashed += other.bytes_hashed;
        self.errors += other.errors;
        self
    }
}

impl std::fmt::Display for ScanStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} new, {} modified, {} unchanged, {} removed across {} dirs ({} manifests written, {} bytes hashed, {} errors)",
            self.new,
            self.modified,
            self.unchanged,
            self.removed,
            self.dirs_scanned,
            self.dirs_written,
            self.bytes_hashed,
            self.errors,
        )
    }
}

fn mtime_ns(md: &fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn flags_for(name: &str, is_symlink: bool, md: &fs::Metadata) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    if is_symlink {
        flags.push("symlink".to_string());
    }
    if name.starts_with('.') {
        flags.push("hidden".to_string());
    }
    if md.permissions().readonly() {
        flags.push("readonly".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if md.permissions().mode() & 0o111 != 0 {
            flags.push("executable".to_string());
        }
    }
    flags.sort();
    flags
}

/// A file queued for hashing, with the stats already gathered.
struct Pending {
    name: String,
    size: u64,
    mtime: u64,
    flags: Vec<String>,
}

/// Decide whether a file needs (re)hashing, per the size/mtime heuristic.
fn needs_hash(old: Option<&FileEntry>, size: u64, mtime: u64, algo: HashAlgo) -> bool {
    match old {
        None => true,
        Some(e) => e.algo != algo.name() || e.size != size || e.mtime_ns != mtime,
    }
}

/// Process a single directory's direct files (non-recursive): update its
/// `.hashit` manifest, dropping entries for files that no longer exist.
/// Returns the stats and whether the manifest changed on disk.
pub fn process_dir(dir: &Path, opts: &ScanOptions, root: &Path) -> Result<(ScanStats, bool)> {
    let old_files: BTreeMap<String, FileEntry> = Manifest::load(dir)?
        .map(|m| m.files)
        .unwrap_or_default();

    // Enumerate direct files (subdirectories get their own manifest).
    let mut current: BTreeMap<String, (bool, fs::Metadata)> = BTreeMap::new();
    let rd = fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))?;
    let mut stats = ScanStats::default();
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                stats.errors += 1;
                if !opts.quiet {
                    eprintln!("hashit: dir entry error in {}: {e:#}", dir.display());
                }
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if is_manifest_file(&name) {
            continue;
        }
        if opts.skip_apple_double && is_apple_double(&name) {
            continue;
        }
        if is_dedup_pointer(&name) {
            continue;
        }
        let path = entry.path();
        if opts.is_excluded(&path, root) {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let is_symlink = ft.is_symlink();
        if ft.is_dir() {
            continue;
        }
        // Skip symlinks unless explicitly following; otherwise the stats (link)
        // and hash (target content) would describe different objects, and the
        // target is typically already inventoried elsewhere under the root.
        if is_symlink && !opts.follow_symlinks {
            continue;
        }
        // entry.metadata() follows symlinks, so a followed link is recorded as
        // its target file.
        let md = match entry.metadata() {
            Ok(md) => md,
            Err(_) => continue,
        };
        if md.is_dir() {
            // followed symlink pointing at a directory
            continue;
        }
        current.insert(name, (is_symlink, md));
    }

    // Plan: reuse hashes where the heuristic allows, collect the rest to hash.
    let mut new_files: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut to_hash: Vec<Pending> = Vec::new();
    for (name, (is_symlink, md)) in &current {
        let size = md.len();
        let mtime = mtime_ns(md);
        let flags = flags_for(name, *is_symlink, md);
        if needs_hash(old_files.get(name), size, mtime, opts.algo) {
            to_hash.push(Pending {
                name: name.clone(),
                size,
                mtime,
                flags,
            });
        } else {
            let mut e = old_files.get(name).unwrap().clone();
            e.flags = flags; // attributes may change without a rehash
            new_files.insert(name.clone(), e);
            stats.unchanged += 1;
            if opts.status {
                print_status("unchanged", &dir.join(name));
            }
        }
    }

    // Hash the pending files in parallel within this directory.
    let algo = opts.algo;
    let hashed: Vec<(Pending, Result<String>)> = to_hash
        .into_par_iter()
        .map(|p| {
            let res = hash_file(&dir.join(&p.name), algo);
            (p, res)
        })
        .collect();

    for (p, res) in hashed {
        match res {
            Ok(hash) => {
                let label = if old_files.contains_key(&p.name) {
                    stats.modified += 1;
                    "modified"
                } else {
                    stats.new += 1;
                    "new"
                };
                stats.bytes_hashed += p.size;
                if opts.status {
                    print_status(label, &dir.join(&p.name));
                } else if opts.verbose && !opts.quiet {
                    println!("  hashed {}", dir.join(&p.name).display());
                }
                new_files.insert(
                    p.name,
                    FileEntry {
                        size: p.size,
                        mtime_ns: p.mtime,
                        hash,
                        algo: algo.name().to_string(),
                        flags: p.flags,
                        hashed_at: now_rfc3339(),
                    },
                );
            }
            Err(e) => {
                stats.errors += 1;
                if !opts.quiet {
                    eprintln!("hashit: error hashing {}: {e:#}", dir.join(&p.name).display());
                }
                // Preserve the prior entry so a transient error doesn't lose data.
                if let Some(old) = old_files.get(&p.name) {
                    new_files.insert(p.name, old.clone());
                }
            }
        }
    }

    for old_name in old_files.keys() {
        if !current.contains_key(old_name) {
            stats.removed += 1;
            if opts.status {
                print_status("removed", &dir.join(old_name));
            }
        }
    }

    stats.dirs_scanned += 1;

    let changed = new_files != old_files;
    if changed {
        stats.dirs_written += 1;
        if !opts.dry_run {
            if new_files.is_empty() {
                Manifest::delete(dir)?;
            } else {
                Manifest::new(new_files).save(dir)?;
            }
        }
    }

    Ok((stats, changed))
}

/// Recursively scan `root`, updating every directory's `.hashit` manifest.
pub fn scan(root: &Path, opts: &ScanOptions) -> Result<ScanStats> {
    let meta = fs::metadata(root).with_context(|| format!("accessing {}", root.display()))?;
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", root.display());
    }

    // Collect every directory under root (pruning excluded subtrees), then
    // process them in parallel.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(opts.follow_symlinks)
        .into_iter()
        .filter_entry(|e| {
            // Never prune the root; prune excluded directories so we don't descend.
            if e.depth() == 0 {
                return true;
            }
            !(e.file_type().is_dir() && opts.is_excluded(e.path(), root))
        });
    for entry in walker {
        match entry {
            Ok(e) if e.file_type().is_dir() => dirs.push(e.into_path()),
            Ok(_) => {}
            Err(e) => {
                if !opts.quiet {
                    eprintln!("hashit: walk error: {e:#}");
                }
            }
        }
    }

    let stats = dirs
        .par_iter()
        .map(|d| match process_dir(d, opts, root) {
            Ok((s, _)) => s,
            Err(e) => {
                if !opts.quiet {
                    eprintln!("hashit: error processing {}: {e:#}", d.display());
                }
                ScanStats {
                    errors: 1,
                    ..Default::default()
                }
            }
        })
        .reduce(ScanStats::default, ScanStats::merge);

    Ok(stats)
}
