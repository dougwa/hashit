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
use clap::Parser;
use tonic::transport::Server;

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
    let args = Args::parse();
    let meta_folder = resolve_meta_folder(args.meta_folder.clone());
    let db = args.db.clone().unwrap_or_else(default_db);

    let mut store = Store::open(&db)?;
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
        std::thread::spawn(move || {
            if let Err(e) = watcher::run(roots, mf, store, debounce) {
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
