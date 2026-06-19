//! `hashit` — the focused command-line scanner.
//!
//! Maintains per-directory `.hashit` manifests of file hashes/stats and offers
//! hash-aware `cp`/`mv`/`rm`. Global views (search, indexing) live in the
//! separate `hashit-idx` daemon; this tool only ever touches individual files
//! under the roots it is given.

mod meta_pass;
mod metacmd;
mod serve;
mod watch;

#[allow(dead_code)] // the generated client is only used by tests.
mod pb_fileops {
    tonic::include_proto!("hashit.fileops.v1");
}

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use glob::Pattern;

use hashit_core::fileops::{self, OpOptions};
use hashit_core::hash::HashAlgo;
use hashit_core::meta;
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
    /// Also run a localhost-only gRPC FileOps server (cp/mv/rm/get-meta/put-meta).
    /// Optional bind address; defaults to 127.0.0.1:50552.
    #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = "127.0.0.1:50552")]
    serve: Option<std::net::SocketAddr>,
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

/// Regenerate metadata artifacts for one changed directory (the `watch` hook).
fn run_dir_meta(mopts: &Option<MetaOptions>, dir: &std::path::Path, quiet: bool) {
    if let Some(m) = mopts {
        if let Err(e) = meta_pass::run_for_dir(dir, m) {
            if !quiet {
                eprintln!("hashit: meta update failed for {}: {e:#}", dir.display());
            }
        }
    }
}

/// Run the watcher (on a background thread, since it blocks) and the gRPC
/// FileOps server (on this thread's async runtime) concurrently.
#[allow(clippy::too_many_arguments)]
fn watch_serve(
    roots: Vec<PathBuf>,
    opts: ScanOptions,
    mopts: Option<MetaOptions>,
    debounce_ms: u64,
    addr: std::net::SocketAddr,
    meta_folder: PathBuf,
    algo: HashAlgo,
) -> Result<()> {
    let quiet = opts.quiet;
    std::thread::spawn(move || {
        if let Err(e) = watch::watch(&roots, &opts, debounce_ms, move |_, dir| {
            run_dir_meta(&mopts, dir, quiet);
        }) {
            eprintln!("hashit: watch stopped: {e:#}");
        }
    });
    if !quiet {
        println!("serving gRPC FileOps on {addr} (localhost only)");
    }
    let rt = tokio::runtime::Runtime::new().context("starting async runtime")?;
    rt.block_on(serve::serve(addr, serve::ServeConfig { meta_folder, algo }))
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
            if let Some(addr) = a.serve {
                let meta_folder = meta::resolve_meta_folder(a.meta.meta_folder.clone());
                watch_serve(
                    a.common.roots.clone(),
                    opts,
                    mopts,
                    a.debounce_ms,
                    addr,
                    meta_folder,
                    a.common.hash,
                )
            } else {
                let roots = a.common.roots.clone();
                let quiet = opts.quiet;
                watch::watch(&roots, &opts, a.debounce_ms, move |_, dir| {
                    run_dir_meta(&mopts, dir, quiet);
                })
            }
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

fn get_meta(a: &GetMetaArgs) -> Result<()> {
    let folder = meta::resolve_meta_folder(a.meta_folder.clone());
    let out = metacmd::gather(&folder, &a.paths)?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn put_meta(a: &PutMetaArgs) -> Result<()> {
    if a.set.is_empty() && a.remove.is_empty() {
        bail!("put-meta: nothing to do (pass --set KEY=VALUE and/or --remove KEY)");
    }
    let sets = metacmd::parse_sets(&a.set)?;
    let folder = meta::resolve_meta_folder(a.meta_folder.clone());
    let updated = metacmd::apply_put(&folder, &a.paths, &sets, &a.remove)?;
    if !a.quiet {
        for (path, hash) in updated {
            println!("updated {path} ({hash})");
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
