//! `hashit` — the focused command-line scanner.
//!
//! Maintains per-directory `.hashit` manifests of file hashes/stats and offers
//! hash-aware `cp`/`mv`/`rm`. Global views (search, indexing) live in the
//! separate `hashit-idx` daemon; this tool only ever touches individual files
//! under the roots it is given.

mod watch;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use glob::Pattern;

use hashit_core::fileops::{self, OpOptions};
use hashit_core::hash::HashAlgo;
use hashit_core::scan::{self, ScanOptions, ScanStats};

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

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Report changes without writing any .hashit files.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct WatchArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Debounce window in milliseconds for coalescing filesystem events.
    #[arg(long, default_value_t = 500)]
    debounce_ms: u64,
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
            Ok(())
        }
        Command::Watch(a) => {
            set_workers(a.common.workers);
            let opts = build_options(&a.common, false)?;
            watch::watch(&a.common.roots, &opts, a.debounce_ms, |_, _| {})
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
    }
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
