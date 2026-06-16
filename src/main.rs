mod dedup;
mod diff;
mod fileops;
mod hash;
mod inventory;
mod manifest;
mod scan;
mod sync;
mod watch;

#[cfg(feature = "extract")]
mod drive;
#[cfg(feature = "extract")]
mod extract;
#[cfg(feature = "extract")]
mod index;
#[cfg(feature = "extract")]
mod store;

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
    /// Scan, then build/refresh the global metadata index (extract once per hash).
    #[cfg(feature = "extract")]
    Index(IndexArgs),
    /// Query the global metadata index (paginated, filterable).
    #[cfg(feature = "extract")]
    Query(QueryArgs),
    /// Inspect or manage indexed drives (list / detach / relabel).
    #[cfg(feature = "extract")]
    Drive(DriveArgs),
    /// Print the cached thumbnail path for a hash (generating it if missing).
    #[cfg(feature = "extract")]
    Thumb(ThumbArgs),
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
    /// Also build/refresh the global metadata index as files change.
    #[cfg(feature = "extract")]
    #[arg(long)]
    index: bool,
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
    /// Substring to ignore anywhere in a path; matching directories are not
    /// recursed. Repeatable, also splits on '|', and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
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
    /// Substring to ignore anywhere in a path; matching directories are not
    /// recursed. Repeatable, also splits on '|', and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
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
    /// Hash algorithm for the scan that precedes aggregation.
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
    /// recursed and matching files are omitted. Repeatable, also splits on '|',
    /// and defaults from $HASHIT_IGNORE.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
    /// Include macOS AppleDouble (._*) sidecar files instead of skipping them.
    #[arg(long)]
    include_apple_double: bool,
    /// Suppress scan progress output (sent to stderr).
    #[arg(long, short)]
    quiet: bool,
    /// Aggregate existing .hashit manifests as-is, without scanning first.
    #[arg(long)]
    no_scan: bool,
    /// Only report files whose content hash occurs more than once.
    #[arg(long)]
    dups_only: bool,
}

#[cfg(feature = "extract")]
#[derive(Args)]
struct IndexArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Index existing .hashit manifests as-is, without scanning first.
    #[arg(long)]
    no_scan: bool,
    /// Re-extract metadata even for content already in the index.
    #[arg(long)]
    reindex: bool,
}

#[cfg(feature = "extract")]
#[derive(Args)]
struct QueryArgs {
    /// Filter by coarse file type category (e.g. image, video, audio).
    #[arg(long = "type", value_name = "CATEGORY")]
    file_type: Option<String>,
    /// Filter by file extension (e.g. cr2, jpg).
    #[arg(long)]
    ext: Option<String>,
    /// Filter by hash prefix.
    #[arg(long)]
    hash: Option<String>,
    /// Filter to content present on a specific drive id.
    #[arg(long)]
    drive: Option<String>,
    /// Only content with no copy on a currently-online drive.
    #[arg(long)]
    offline: bool,
    /// Filter to content carrying this metadata key.
    #[arg(long)]
    key: Option<String>,
    /// Together with --key, require this value.
    #[arg(long, requires = "key")]
    value: Option<String>,
    /// Max rows to return.
    #[arg(long, default_value_t = 100)]
    limit: i64,
    /// Rows to skip (for pagination).
    #[arg(long, default_value_t = 0)]
    offset: i64,
    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    /// Write to a file instead of stdout.
    #[arg(long, short)]
    output: Option<PathBuf>,
}

#[cfg(feature = "extract")]
#[derive(Args)]
struct DriveArgs {
    #[command(subcommand)]
    cmd: DriveCmd,
}

#[cfg(feature = "extract")]
#[derive(Subcommand)]
enum DriveCmd {
    /// List indexed drives and their online/offline status.
    List,
    /// Permanently detach a drive: purge its locations and orphaned metadata.
    Detach {
        /// Drive id (from `hashit drive list`).
        drive_id: String,
    },
    /// Change a drive's label.
    Relabel {
        drive_id: String,
        label: String,
    },
}

#[cfg(feature = "extract")]
#[derive(Args)]
struct ThumbArgs {
    /// Content hash (full, or a unique-enough prefix).
    hash: String,
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
            set_workers(a.workers);
            let scan_opts = ScanOptions {
                algo: a.hash,
                follow_symlinks: a.follow_symlinks,
                excludes: parse_excludes(&a.excludes)?,
                ignores: collect_ignores(&a.ignores),
                quiet: a.quiet,
                verbose: false,
                status: false,
                skip_apple_double: !a.include_apple_double,
                dry_run: false,
            };
            let mut records = Vec::new();
            for path in &a.paths {
                if !a.no_scan {
                    scan::scan(path, &scan_opts)?;
                }
                records.extend(inventory::build_inventory(path, &scan_opts)?);
            }
            records.sort_by(|x, y| x.path.cmp(&y.path));
            if a.dups_only {
                inventory::retain_dups(&mut records);
            }
            inventory::write_inventory(&records, a.format, a.output.as_deref())?;
            Ok(())
        }
        Command::Watch(a) => {
            set_workers(a.common.workers);
            let opts = build_options(&a.common, false)?;
            #[cfg(feature = "extract")]
            if a.index {
                // Seed the index with the current state, then keep it in sync as
                // the watcher reconciles each changed directory.
                index::index(&a.common.roots, &opts, false, false)?;
                let mut indexer = index::Indexer::new()?;
                let optsref = &opts;
                return watch::watch(&a.common.roots, &opts, a.debounce_ms, |root, dir| {
                    if let Err(e) = indexer.reconcile_dir(root, dir, optsref) {
                        if !optsref.quiet {
                            eprintln!("hashit: index update failed for {}: {e:#}", dir.display());
                        }
                    }
                });
            }
            watch::watch(&a.common.roots, &opts, a.debounce_ms, |_, _| {})
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
                ignores: collect_ignores(&a.ignores),
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
                ignores: collect_ignores(&a.ignores),
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
        #[cfg(feature = "extract")]
        Command::Index(a) => {
            set_workers(a.common.workers);
            let opts = build_options(&a.common, false)?;
            let stats = index::index(&a.common.roots, &opts, a.no_scan, a.reindex)?;
            if !a.common.quiet {
                println!(
                    "indexed {} files, {} new content extracted, {} thumbnails",
                    stats.files, stats.new_content, stats.thumbs
                );
            }
            Ok(())
        }
        #[cfg(feature = "extract")]
        Command::Query(a) => {
            let store = store::Store::open_default()?;
            // Keep online/offline accurate before reporting it.
            drive::refresh_presence(&store)?;
            let filter = store::QueryFilter {
                file_type: a.file_type,
                ext: a.ext,
                hash_prefix: a.hash,
                drive_id: a.drive,
                offline_only: a.offline,
                key: a.key,
                value: a.value,
                limit: a.limit,
                offset: a.offset,
            };
            let rows = store.query(&filter)?;
            write_query(&rows, a.format, a.output.as_deref())?;
            Ok(())
        }
        #[cfg(feature = "extract")]
        Command::Drive(a) => {
            let mut store = store::Store::open_default()?;
            match a.cmd {
                DriveCmd::List => {
                    drive::refresh_presence(&store)?;
                    for d in store.list_drives()? {
                        println!(
                            "{}  {:<7}  {} files  label={:?}  {}",
                            d.drive_id,
                            if d.online { "online" } else { "offline" },
                            d.files,
                            d.label,
                            d.last_root
                        );
                    }
                }
                DriveCmd::Detach { drive_id } => {
                    let n = store.detach_drive(&drive_id)?;
                    println!("detached {drive_id}: removed {n} locations");
                }
                DriveCmd::Relabel { drive_id, label } => {
                    if !store.relabel_drive(&drive_id, &label)? {
                        anyhow::bail!("no such drive: {drive_id}");
                    }
                }
            }
            Ok(())
        }
        #[cfg(feature = "extract")]
        Command::Thumb(a) => {
            let store = store::Store::open_default()?;
            match store.thumb_lookup(&a.hash)? {
                None => anyhow::bail!("no indexed content matching hash {}", a.hash),
                Some((algo, hash, has_thumb, src)) => {
                    let dest = store.thumb_path(&algo, &hash);
                    if !has_thumb || !dest.exists() {
                        if extract::make_thumb(&src, &dest) {
                            store.set_has_thumb(&algo, &hash, true)?;
                        } else {
                            anyhow::bail!(
                                "could not generate thumbnail for {hash} (source: {})",
                                src.display()
                            );
                        }
                    }
                    println!("{}", dest.display());
                }
            }
            Ok(())
        }
    }
}

/// Render query results to stdout or a file (mirrors `inventory::write_inventory`).
#[cfg(feature = "extract")]
fn write_query(
    rows: &[store::QueryRow],
    format: Format,
    out: Option<&std::path::Path>,
) -> Result<()> {
    use std::io::Write;
    let mut sink: Box<dyn Write> = match out {
        Some(p) => {
            Box::new(std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?)
        }
        None => Box::new(std::io::stdout().lock()),
    };
    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(rows).context("serializing query results")?;
            sink.write_all(json.as_bytes())?;
            sink.write_all(b"\n")?;
        }
        Format::Csv => {
            let mut wtr = csv::Writer::from_writer(&mut sink);
            for r in rows {
                wtr.serialize(r).context("writing csv row")?;
            }
            wtr.flush()?;
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
