//! `hashit-idx` — a read-only global index and local gRPC search service.
//!
//! Given a set of roots and the shared metadata folder, it builds a SQLite
//! index from the `.hashit` manifests and `.hashit-meta/*.meta.json` files,
//! keeps it fresh by watching those files, and serves search over a
//! localhost-only gRPC endpoint. It never writes the source files — all updates
//! flow through `hashit`.

mod query;
mod search;
mod store;
mod watcher;

mod pb {
    tonic::include_proto!("hashit.search.v1");
}

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tonic::transport::Server;

use hashit_core::manifest::ns_to_rfc3339;
use hashit_core::meta::resolve_meta_folder;

use crate::pb::search_server::SearchServer;
use crate::search::SearchService;
use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "hashit-idx",
    version,
    about = "Read-only global index + local gRPC search over .hashit and .hashit-meta files."
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index the roots, watch them for changes, and serve gRPC search.
    Serve(ServeArgs),
    /// Run a query-language search against the existing index and print matches.
    Query(QueryArgs),
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Roots to index and watch for .hashit changes.
    #[arg(required = true, num_args = 1.., value_name = "ROOT")]
    roots: Vec<PathBuf>,
    /// Metadata folder to index and watch (default $HASHIT_META or ~/.hashit-meta).
    #[arg(long, value_name = "PATH")]
    meta_folder: Option<PathBuf>,
    /// SQLite index database path (default $HASHIT_HOME/index.db or ~/.hashit/index.db).
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,
    /// Local address to serve gRPC on.
    #[arg(long, default_value = "127.0.0.1:50551")]
    addr: SocketAddr,
    /// Debounce window in milliseconds for coalescing filesystem events.
    #[arg(long, default_value_t = 500)]
    debounce_ms: u64,
    /// Substring to ignore anywhere in a path; matching directories (and their
    /// subtrees) are not indexed or watched. Repeatable; also reads
    /// `$HASHIT_IGNORE`. Both sources may bundle multiple values with '|'.
    #[arg(long = "ignore", value_name = "STR")]
    ignores: Vec<String>,
}

#[derive(clap::Args)]
struct QueryArgs {
    /// The query string (see QUERY.md). Empty matches everything.
    // allow_hyphen_values so a leading `-` (must-not) term isn't read as a flag.
    #[arg(value_name = "QUERY", default_value = "", allow_hyphen_values = true)]
    query: String,
    /// SQLite index database path (default $HASHIT_HOME/index.db or ~/.hashit/index.db).
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,
    /// Maximum rows to print (0 = server default of 100).
    #[arg(long, default_value_t = 0)]
    limit: u32,
    /// Rows to skip, for pagination.
    #[arg(long, default_value_t = 0)]
    offset: u32,
    /// Print only the matching paths, one per line.
    #[arg(long)]
    paths_only: bool,
}

/// Merge ignore substrings from `$HASHIT_IGNORE` and the repeatable `--ignore`
/// flag. Both sources split on '|'; empty segments are dropped (an empty
/// substring would otherwise match every path). Mirrors `hashit`.
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

/// Default index database location: `$HASHIT_HOME/index.db`, else
/// `~/.hashit/index.db`, else `./.hashit-index.db`.
fn default_db() -> PathBuf {
    if let Ok(home) = std::env::var("HASHIT_HOME") {
        return PathBuf::from(home).join("index.db");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".hashit").join("index.db");
    }
    PathBuf::from(".hashit-index.db")
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::Query(args) => run_query(args),
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let meta_folder = resolve_meta_folder(args.meta_folder.clone());
    let db = args.db.clone().unwrap_or_else(default_db);
    let ignores = collect_ignores(&args.ignores);

    let mut store = Store::open(&db)?;
    store.set_ignores(ignores.clone());
    eprintln!("hashit-idx: building index at {} …", db.display());
    store.rebuild(&args.roots, &meta_folder)?;
    let (files, hashes, tags) = store.stats()?;
    eprintln!("hashit-idx: indexed {files} files, {hashes} hashes, {tags} tag/property rows");

    let store = Arc::new(Mutex::new(store));

    // Watch the source files on a dedicated thread; notify owns its own threads
    // and feeds this loop via a channel.
    {
        let store = store.clone();
        let roots = args.roots.clone();
        let mf = meta_folder.clone();
        let debounce = args.debounce_ms;
        let ignores = ignores.clone();
        std::thread::spawn(move || {
            if let Err(e) = watcher::run(roots, mf, store, debounce, ignores) {
                eprintln!("hashit-idx: watcher stopped: {e:#}");
            }
        });
    }

    eprintln!("hashit-idx: serving gRPC on {} (read-only)", args.addr);
    Server::builder()
        .add_service(SearchServer::new(SearchService { store }))
        .serve(args.addr)
        .await
        .context("serving gRPC")?;
    Ok(())
}

/// Read the existing index and run a query-language search. Reads what's there;
/// keeping the index fresh is the running `serve` daemon's job.
fn run_query(args: QueryArgs) -> Result<()> {
    let db = args.db.unwrap_or_else(default_db);
    let store = Store::open(&db).with_context(|| format!("opening index {}", db.display()))?;

    let ast = query::parse(&args.query).context("parsing query")?;
    let mut filter = query::lower(&ast, chrono::Local::now()).context("building query")?;
    filter.limit = args.limit;
    filter.offset = args.offset;

    let (rows, total) = store.query_filter(&filter)?;
    for r in &rows {
        if args.paths_only {
            println!("{}", r.path);
        } else {
            let kind = r.file_type.as_deref().unwrap_or("-");
            println!(
                "{}  {:>12}  {:<7}  {}",
                ns_to_rfc3339(r.mtime_ns),
                r.size,
                kind,
                r.path
            );
        }
    }
    eprintln!("{total} match(es); showing {}", rows.len());
    Ok(())
}
