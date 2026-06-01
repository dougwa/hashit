mod dedup;
mod diff;
mod fileops;
mod hash;
mod inventory;
mod manifest;
mod scan;
mod sync;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use glob::Pattern;

use dedup::DedupOptions;
use diff::{DiffFormat, DiffOptions};
use fileops::OpOptions;
use hash::HashAlgo;
use inventory::Format;
use scan::ScanOptions;
use sync::{Direction, SyncOptions};

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
    /// Aggregate every .hashit under a path into a single inventory report.
    Inventory(InventoryArgs),
    /// Run a scan, then watch the path and update manifests in real time.
    Watch(WatchArgs),
    /// Scan, then find duplicate files by hash and remove all but one of each.
    Dedup(DedupArgs),
    /// Compare two paths by content hash: which hashes are unique to each side.
    Diff(DiffArgs),
    /// Copy files that are missing (by content hash) between two paths.
    Sync(SyncArgs),
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
struct DedupArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Prompt which file to keep for each duplicate set (default if neither given).
    #[arg(short, long, conflicts_with = "auto")]
    interactive: bool,
    /// Auto-keep the best file: non-hidden, then fewest '/', then alphabetical.
    #[arg(short, long)]
    auto: bool,
    /// Do not leave <file>.dedup pointer files for removed duplicates.
    #[arg(long)]
    no_dedup_link: bool,
    /// Show what would be removed without deleting anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct DiffArgs {
    /// First path to compare.
    path1: PathBuf,
    /// Second path to compare.
    path2: PathBuf,
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
    /// Include macOS AppleDouble (._*) sidecar files instead of skipping them.
    #[arg(long)]
    include_apple_double: bool,
    /// Suppress scan progress output (sent to stderr).
    #[arg(long, short)]
    quiet: bool,
    /// Output format.
    #[arg(long, value_enum, default_value_t = DiffFormat::Grouped)]
    format: DiffFormat,
    /// Also report hashes present in both paths.
    #[arg(long)]
    show_common: bool,
    /// Compare existing .hashit manifests as-is, without scanning first.
    #[arg(long)]
    no_scan: bool,
}

#[derive(Args)]
struct SyncArgs {
    /// First path.
    path1: PathBuf,
    /// Second path.
    path2: PathBuf,
    /// Direction: 'to' (path1->path2, default), 'from' (path2->path1), or 'both'.
    #[arg(short, long, value_enum, default_value_t = Direction::To)]
    direction: Direction,
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
    /// Include macOS AppleDouble (._*) sidecar files instead of skipping them.
    #[arg(long)]
    include_apple_double: bool,
    /// Suppress scan progress and per-file output.
    #[arg(long, short)]
    quiet: bool,
    /// Compare existing .hashit manifests as-is, without scanning first.
    #[arg(long)]
    no_scan: bool,
    /// Show what would be copied without writing anything.
    #[arg(long)]
    dry_run: bool,
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

#[derive(Args)]
struct InventoryArgs {
    /// One or more starting paths to aggregate.
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    /// Write to a file instead of stdout.
    #[arg(long, short)]
    output: Option<PathBuf>,
}

fn parse_excludes(globs: &[String]) -> Result<Vec<Pattern>> {
    let mut excludes = Vec::with_capacity(globs.len());
    for g in globs {
        excludes.push(Pattern::new(g).with_context(|| format!("invalid exclude glob: {g}"))?);
    }
    Ok(excludes)
}

fn build_options(c: &CommonArgs, dry_run: bool) -> Result<ScanOptions> {
    Ok(ScanOptions {
        algo: c.hash,
        follow_symlinks: c.follow_symlinks,
        excludes: parse_excludes(&c.excludes)?,
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
fn fileop_scan_opts(hash: HashAlgo, quiet: bool) -> ScanOptions {
    ScanOptions {
        algo: hash,
        follow_symlinks: false,
        excludes: Vec::new(),
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
            let mut stats = scan::ScanStats::default();
            for root in &a.common.roots {
                stats = stats.merge(scan::scan(root, &opts)?);
            }
            if !opts.quiet {
                let prefix = if a.dry_run { "dry-run: " } else { "" };
                println!("{prefix}{stats}");
            }
            Ok(())
        }
        Command::Inventory(a) => {
            let mut records = Vec::new();
            for path in &a.paths {
                records.extend(inventory::build_inventory(path)?);
            }
            records.sort_by(|x, y| x.path.cmp(&y.path));
            inventory::write_inventory(&records, a.format, a.output.as_deref())?;
            Ok(())
        }
        Command::Watch(a) => {
            set_workers(a.common.workers);
            let opts = build_options(&a.common, false)?;
            watch::watch(&a.common.roots, &opts, a.debounce_ms)
        }
        Command::Dedup(a) => {
            set_workers(a.common.workers);
            // The scan inside dedup must always write (dry-run only gates removals).
            let scan_opts = build_options(&a.common, false)?;
            let dd = DedupOptions {
                interactive: !a.auto,
                write_links: !a.no_dedup_link,
                dry_run: a.dry_run,
            };
            dedup::dedup(&a.common.roots, &scan_opts, &dd)
        }
        Command::Diff(a) => {
            set_workers(a.workers);
            let scan_opts = ScanOptions {
                algo: a.hash,
                follow_symlinks: a.follow_symlinks,
                excludes: parse_excludes(&a.excludes)?,
                quiet: a.quiet,
                verbose: false,
                status: false,
                skip_apple_double: !a.include_apple_double,
                dry_run: false,
            };
            let dd = DiffOptions {
                format: a.format,
                show_common: a.show_common,
                scan_first: !a.no_scan,
            };
            diff::diff(&a.path1, &a.path2, &scan_opts, &dd)
        }
        Command::Sync(a) => {
            set_workers(a.workers);
            let scan_opts = ScanOptions {
                algo: a.hash,
                follow_symlinks: a.follow_symlinks,
                excludes: parse_excludes(&a.excludes)?,
                quiet: a.quiet,
                verbose: false,
                status: false,
                skip_apple_double: !a.include_apple_double,
                dry_run: false,
            };
            let so = SyncOptions {
                direction: a.direction,
                scan_first: !a.no_scan,
                dry_run: a.dry_run,
            };
            sync::sync(&a.path1, &a.path2, &scan_opts, &so)
        }
        Command::Cp(a) => {
            set_workers(a.workers);
            let scan_opts = fileop_scan_opts(a.hash, a.quiet);
            let opts = OpOptions {
                recursive: a.recursive,
                force: a.force,
                quiet: a.quiet,
            };
            fileops::cp(&a.paths, &opts, &scan_opts)
        }
        Command::Mv(a) => {
            set_workers(a.workers);
            let scan_opts = fileop_scan_opts(a.hash, a.quiet);
            let opts = OpOptions {
                recursive: a.recursive,
                force: a.force,
                quiet: a.quiet,
            };
            fileops::mv(&a.paths, &opts, &scan_opts)
        }
        Command::Rm(a) => {
            set_workers(a.workers);
            let scan_opts = fileop_scan_opts(a.hash, a.quiet);
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
