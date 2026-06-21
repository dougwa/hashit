//! Keep the index fresh by watching the source files.
//!
//! Only two kinds of change matter: a `.hashit` manifest (re-sync that
//! directory's files) and a `<hash>.meta.json` (re-sync that hash's
//! content/tags). Thumbnail/preview JPEGs are deliberately ignored — their
//! presence isn't part of the searchable index.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use hashit_core::manifest::MANIFEST_NAME;
use hashit_core::walk;

use crate::store::{hash_from_meta_filename, Store};

/// Watch the roots (for `.hashit`) and the meta folder (for `*.meta.json`),
/// applying incremental updates to the shared store. Blocks until the channel
/// closes. Directories matching one of the `ignores` substrings are skipped, to
/// match the build-time `--ignore` filtering.
pub fn run(
    roots: Vec<PathBuf>,
    meta_folder: PathBuf,
    store: Arc<Mutex<Store>>,
    debounce_ms: u64,
    ignores: Vec<String>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(Duration::from_millis(debounce_ms), None, move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;

    let mut targets: Vec<&PathBuf> = roots.iter().collect();
    if !roots.contains(&meta_folder) {
        targets.push(&meta_folder);
    }
    for target in targets {
        if target.exists() {
            debouncer
                .watcher()
                .watch(target, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", target.display()))?;
            debouncer.cache().add_root(target, RecursiveMode::Recursive);
        }
    }

    for res in rx {
        let events = match res {
            Ok(events) => events,
            Err(errors) => {
                for e in errors {
                    eprintln!("hashit-idx: watch error: {e:#}");
                }
                continue;
            }
        };

        // Coalesce a batch into the set of dirs and hashes that need re-syncing.
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        let mut hashes: BTreeSet<String> = BTreeSet::new();
        for ev in &events {
            for path in &ev.event.paths {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name == MANIFEST_NAME {
                    if let Some(parent) = path.parent() {
                        // Skip dirs under an ignored path, mirroring the build.
                        let ignored = roots
                            .iter()
                            .filter(|r| parent.starts_with(r))
                            .any(|r| walk::is_ignored(parent, r, &ignores));
                        if !ignored {
                            dirs.insert(parent.to_path_buf());
                        }
                    }
                } else if let Some(h) = hash_from_meta_filename(&name) {
                    hashes.insert(h.to_string());
                }
            }
        }

        if dirs.is_empty() && hashes.is_empty() {
            continue;
        }
        let mut store = match store.lock() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("hashit-idx: store lock poisoned; stopping watcher");
                break;
            }
        };
        for dir in &dirs {
            if let Err(e) = store.sync_dir(dir) {
                eprintln!("hashit-idx: failed to sync {}: {e:#}", dir.display());
            }
        }
        for hash in &hashes {
            if let Err(e) = store.sync_meta(&meta_folder, hash) {
                eprintln!("hashit-idx: failed to sync meta {hash}: {e:#}");
            }
        }
    }

    Ok(())
}
