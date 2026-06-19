//! `hashit` — the focused command-line scanner.
//!
//! Maintains per-directory `.hashit` manifests of file hashes/stats and offers
//! hash-aware `cp`/`mv`/`rm`. Global views (search, indexing) live in the
//! separate `hashit-idx` daemon; this tool only ever touches individual files
//! under the roots it is given.

mod meta_pass;
mod watch;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use glob::Pattern;
use serde::Serialize;

use hashit_core::fileops::{self, OpOptions};
use hashit_core::hash::HashAlgo;
use hashit_core::manifest::ManifestCache;
use hashit_core::meta::{self, MetaFile};
use hashit_core::scan::{self, ScanOptions, ScanStats};
use meta_pass::MetaOptions;

#[derive(Parser)]
#[command(
    name = "hashit",
    version,
    about = "Maintain per-directory .hashit manifests of file hashes and stats."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Traverse a path and update .hashit manifests (rehashing only changed files).
    Scan(ScanArgs),
    /// Run a scan, then watch the path and update manifests in real time.
    Watch(WatchArgs),
    /// Copy files/directories (like cp), updating target .hashit manifests.
    Cp(CpArgs),
    /// Move/rename files/directories (like mv), updating .hashit manifests.
    Mv(MvArgs),
    /// Remove files/directories (like rm), updating source .hashit manifests.
    Rm(RmArgs),
    /// Show metadata for path(s): tags, properties, and thumbnail/preview paths.
    GetMeta(GetMetaArgs),
    /// Set or remove user-authored metadata properties for path(s).
    PutMeta(PutMetaArgs),
}

#[derive(Args)]
struct CommonArgs {
    /// One or more starting paths to traverse.
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    roots: Vec<PathBuf>,
    /// Hash algorithm.
    #[arg(long, value_enum, default_value_t = HashAlgo::Blake3)]
    hash: HashAlgo,
    /// Number of hashing threads (0 = number of CPUs).
    #[arg(long, default_value_t = 0)]
    workers: usize,
    /// Follow symbolic links.
    #[arg(long)]
    follow_symlinks: bool,
    /// Glob to exclude (matched against basename and path relative to root). Repeatable.
    #[arg(long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,
    /// Substring to ignore anywhere in a path; matching directories are not
    /// recursed. Repeatable, also splits on '|', and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
    /// Suppress per-file and error output.
    #[arg(long, short)]
    quiet: bool,
    /// Print each file as it is hashed.
    #[arg(long, short)]
    verbose: bool,
    /// Print a status line (new/modified/unchanged/removed) for every file processed.
    #[arg(long)]
    status: bool,
    /// Include macOS AppleDouble (._*) sidecar files instead of skipping them.
    #[arg(long)]
    include_apple_double: bool,
}

/// Flags controlling per-hash metadata artifacts, shared by `scan` and `watch`.
#[derive(Args)]
struct MetaArgs {
    /// Generate a 512px thumbnail for each content hash that lacks one.
    #[arg(long)]
    meta_thumbnail: bool,
    /// Generate a 2048px preview for each content hash that lacks one.
    #[arg(long)]
    meta_preview: bool,
    /// Extract EXIF tags into <hash>.meta.json for each hash that lacks them.
    #[arg(long)]
    meta_tags: bool,
    /// Shorthand for --meta-thumbnail --meta-preview --meta-tags.
    #[arg(long)]
    meta_all: bool,
    /// Folder holding sharded per-hash metadata (default $HASHIT_META or ~/.hashit-meta).
    #[arg(long, value_name = "PATH")]
    meta_folder: Option<PathBuf>,
}

impl MetaArgs {
    /// Resolve the requested artifacts, or `None` when no `--meta-*` flag is set.
    fn to_options(&self) -> Option<MetaOptions> {
        let (thumbnail, preview, tags) = (
            self.meta_thumbnail || self.meta_all,
            self.meta_preview || self.meta_all,
            self.meta_tags || self.meta_all,
        );
        if !(thumbnail || preview || tags) {
            return None;
        }
        Some(MetaOptions {
            folder: meta::resolve_meta_folder(self.meta_folder.clone()),
            thumbnail,
            preview,
            tags,
        })
    }
}

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    meta: MetaArgs,
    /// Report changes without writing any .hashit files.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct WatchArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    meta: MetaArgs,
    /// Debounce window in milliseconds for coalescing filesystem events.
    #[arg(long, default_value_t = 500)]
    debounce_ms: u64,
}

#[derive(Args)]
struct GetMetaArgs {
    /// File path(s) to look up (must already be recorded in a .hashit manifest).
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Folder holding sharded per-hash metadata (default $HASHIT_META or ~/.hashit-meta).
    #[arg(long, value_name = "PATH")]
    meta_folder: Option<PathBuf>,
}

#[derive(Args)]
struct PutMetaArgs {
    /// File path(s) to update (must already be recorded in a .hashit manifest).
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Set a user property, KEY=VALUE. Repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,
    /// Remove a user property by KEY. Repeatable.
    #[arg(long = "remove", value_name = "KEY")]
    remove: Vec<String>,
    /// Folder holding sharded per-hash metadata (default $HASHIT_META or ~/.hashit-meta).
    #[arg(long, value_name = "PATH")]
    meta_folder: Option<PathBuf>,
    /// Suppress per-path output.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct CpArgs {
    /// One or more sources followed by the destination (last argument).
    #[arg(required = true, num_args = 2.., value_name = "SRC... DEST")]
    paths: Vec<PathBuf>,
    /// Recurse into directories.
    #[arg(short, long)]
    recursive: bool,
    /// Overwrite existing files in the target.
    #[arg(short, long)]
    force: bool,
    /// Substring to ignore anywhere in a path during the on-demand scan;
    /// matching directories are not recursed. Repeatable, also splits on '|',
    /// and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
    /// Hash algorithm for files lacking a source manifest entry.
    #[arg(long, value_enum, default_value_t = HashAlgo::Blake3)]
    hash: HashAlgo,
    /// Number of hashing threads (0 = number of CPUs).
    #[arg(long, default_value_t = 0)]
    workers: usize,
    /// Suppress per-file output.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct MvArgs {
    /// One or more sources followed by the destination (last argument).
    #[arg(required = true, num_args = 2.., value_name = "SRC... DEST")]
    paths: Vec<PathBuf>,
    /// Accepted for symmetry; directories are moved regardless.
    #[arg(short, long)]
    recursive: bool,
    /// Overwrite an existing destination.
    #[arg(short, long)]
    force: bool,
    /// Substring to ignore anywhere in a path during the on-demand scan;
    /// matching directories are not recursed. Repeatable, also splits on '|',
    /// and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
    /// Hash algorithm for files lacking a source manifest entry.
    #[arg(long, value_enum, default_value_t = HashAlgo::Blake3)]
    hash: HashAlgo,
    /// Number of hashing threads (0 = number of CPUs).
    #[arg(long, default_value_t = 0)]
    workers: usize,
    /// Suppress per-file output.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Args)]
struct RmArgs {
    /// Files/directories to remove.
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Recurse into directories.
    #[arg(short, long)]
    recursive: bool,
    /// Ignore nonexistent files; never error on missing operands.
    #[arg(short, long)]
    force: bool,
    /// Substring to ignore anywhere in a path during the on-demand scan;
    /// matching directories are not recursed. Repeatable, also splits on '|',
    /// and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
    /// Hash algorithm used when reconciling affected manifests.
    #[arg(long, value_enum, default_value_t = HashAlgo::Blake3)]
    hash: HashAlgo,
    /// Number of hashing threads (0 = number of CPUs).
    #[arg(long, default_value_t = 0)]
    workers: usize,
    /// Suppress per-file output.
    #[arg(short, long)]
    quiet: bool,
}

fn parse_excludes(globs: &[String]) -> Result<Vec<Pattern>> {
    let mut excludes = Vec::with_capacity(globs.len());
    for g in globs {
        excludes.push(Pattern::new(g).with_context(|| format!("invalid exclude glob: {g}"))?);
    }
    Ok(excludes)
}

/// Merge ignore substrings from `$HASHIT_IGNORE` and the repeatable `--ignore`
/// flag. Both sources split on '|'; empty segments are dropped (an empty
/// substring would otherwise match every path).
fn collect_ignores(cli: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(env) = std::env::var("HASHIT_IGNORE") {
        out.extend(env.split('|').map(str::to_string));
    }
    for v in cli {
        out.extend(v.split('|').map(str::to_string));
    }
    out.retain(|s| !s.is_empty());
    out
}

fn build_options(c: &CommonArgs, dry_run: bool) -> Result<ScanOptions> {
    Ok(ScanOptions {
        algo: c.hash,
        follow_symlinks: c.follow_symlinks,
        excludes: parse_excludes(&c.excludes)?,
        ignores: collect_ignores(&c.ignores),
        quiet: c.quiet,
        verbose: c.verbose,
        status: c.status,
        skip_apple_double: !c.include_apple_double,
        dry_run,
    })
}

fn set_workers(n: usize) {
    if n > 0 {
        let _ = rayon::ThreadPoolBuilder::new().num_threads(n).build_global();
    }
}

/// Minimal scan options for the file ops (cp/mv/rm): they only need the algo
/// for reconciling affected manifests, and quiet to gate output.
fn fileop_scan_opts(hash: HashAlgo, quiet: bool, ignores: Vec<String>) -> ScanOptions {
    ScanOptions {
        algo: hash,
        follow_symlinks: false,
        excludes: Vec::new(),
        ignores,
        quiet,
        verbose: false,
        status: false,
        skip_apple_double: true,
        dry_run: false,
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Scan(a) => {
            set_workers(a.common.workers);
            let opts = build_options(&a.common, a.dry_run)?;
            let mut stats = ScanStats::default();
            for root in &a.common.roots {
                stats = stats.merge(scan::scan(root, &opts)?);
            }
            if !opts.quiet {
                let prefix = if a.dry_run { "dry-run: " } else { "" };
                println!("{prefix}{stats}");
            }
            // The metadata pass reads the manifests just written, so skip it on a
            // dry run (nothing was persisted to derive artifacts from).
            if !a.dry_run {
                if let Some(mopts) = a.meta.to_options() {
                    meta_pass::run(&a.common.roots, &mopts, opts.quiet)?;
                }
            }
            Ok(())
        }
        Command::Watch(a) => {
            set_workers(a.common.workers);
            let opts = build_options(&a.common, false)?;
            let mopts = a.meta.to_options();
            // Seed metadata for the initial scan state, then keep it fresh as
            // each changed directory is reconciled.
            if let Some(m) = &mopts {
                meta_pass::run(&a.common.roots, m, opts.quiet)?;
            }
            watch::watch(&a.common.roots, &opts, a.debounce_ms, |_, dir| {
                if let Some(m) = &mopts {
                    if let Err(e) = meta_pass::run_for_dir(dir, m) {
                        if !opts.quiet {
                            eprintln!("hashit: meta update failed for {}: {e:#}", dir.display());
                        }
                    }
                }
            })
        }
        Command::Cp(a) => {
            set_workers(a.workers);
            let scan_opts = fileop_scan_opts(a.hash, a.quiet, collect_ignores(&a.ignores));
            let opts = OpOptions {
                recursive: a.recursive,
                force: a.force,
                quiet: a.quiet,
            };
            fileops::cp(&a.paths, &opts, &scan_opts)
        }
        Command::Mv(a) => {
            set_workers(a.workers);
            let scan_opts = fileop_scan_opts(a.hash, a.quiet, collect_ignores(&a.ignores));
            let opts = OpOptions {
                recursive: a.recursive,
                force: a.force,
                quiet: a.quiet,
            };
            fileops::mv(&a.paths, &opts, &scan_opts)
        }
        Command::Rm(a) => {
            set_workers(a.workers);
            let scan_opts = fileop_scan_opts(a.hash, a.quiet, collect_ignores(&a.ignores));
            let opts = OpOptions {
                recursive: a.recursive,
                force: a.force,
                quiet: a.quiet,
            };
            fileops::rm(&a.paths, &opts, &scan_opts)
        }
        Command::GetMeta(a) => get_meta(&a),
        Command::PutMeta(a) => put_meta(&a),
    }
}

/// Resolve a file to its recorded `(hash, algo, size)` from the manifest in its
/// parent directory. Errors if the file isn't recorded yet.
fn entry_for(cache: &mut ManifestCache, path: &Path) -> Result<(String, String, u64)> {
    match cache.entry_for(path) {
        Some(e) => Ok((e.hash, e.algo, e.size)),
        None => bail!(
            "{}: no .hashit entry found; run `hashit scan` on its directory first",
            path.display()
        ),
    }
}

/// One path's metadata in the `get-meta` JSON output. Absent fields are omitted;
/// a lookup failure is reported via `error`.
#[derive(Default, Serialize)]
struct MetaView {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    algo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ext: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    properties: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<String>,
}

fn get_meta(a: &GetMetaArgs) -> Result<()> {
    let folder = meta::resolve_meta_folder(a.meta_folder.clone());
    let mut cache = ManifestCache::default();
    let mut out: Vec<MetaView> = Vec::with_capacity(a.paths.len());
    for path in &a.paths {
        let mut view = MetaView {
            path: path.display().to_string(),
            ..Default::default()
        };
        match entry_for(&mut cache, path) {
            Err(e) => view.error = Some(format!("{e:#}")),
            Ok((hash, algo, size)) => {
                if let Some(m) = MetaFile::load(&folder, &hash)? {
                    view.file_type = m.file_type;
                    view.ext = m.ext;
                    view.tags = m.tags;
                    view.properties = m.properties;
                }
                let thumb = meta::thumbnail_path(&folder, &hash);
                if thumb.exists() {
                    view.thumbnail = Some(thumb.display().to_string());
                }
                let preview = meta::preview_path(&folder, &hash);
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
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn put_meta(a: &PutMetaArgs) -> Result<()> {
    if a.set.is_empty() && a.remove.is_empty() {
        bail!("put-meta: nothing to do (pass --set KEY=VALUE and/or --remove KEY)");
    }
    // Parse KEY=VALUE pairs up front so a malformed one fails before any write.
    let sets: Vec<(String, String)> = a
        .set
        .iter()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => bail!("invalid --set value (expected KEY=VALUE): {kv}"),
        })
        .collect::<Result<_>>()?;

    let folder = meta::resolve_meta_folder(a.meta_folder.clone());
    let mut cache = ManifestCache::default();
    // Metadata is keyed by content hash, so edit each unique hash once even if
    // several of the given paths share it.
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    for path in &a.paths {
        let (hash, algo, size) = entry_for(&mut cache, path)?;
        if !done.insert(hash.clone()) {
            continue;
        }
        let mut m = MetaFile::load(&folder, &hash)?.unwrap_or_default();
        m.hash = hash.clone();
        if m.algo.is_empty() {
            m.algo = algo;
        }
        if m.size == 0 {
            m.size = size;
        }
        for (k, v) in &sets {
            m.properties.insert(k.clone(), v.clone());
        }
        for k in &a.remove {
            m.properties.remove(k);
        }
        m.save(&folder)?;
        if !a.quiet {
            println!("updated {} ({hash})", path.display());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hashit: {e:#}");
            ExitCode::FAILURE
        }
    }
}
