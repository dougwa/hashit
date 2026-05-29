use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::manifest::is_manifest_file;
use crate::scan::{is_apple_double, process_dir, scan, ScanOptions};

/// Run a full scan, then watch `root` recursively and keep `.hashit` manifests
/// updated as files are added, changed, or removed. Blocks until interrupted.
pub fn watch(root: &std::path::Path, opts: &ScanOptions, debounce_ms: u64) -> Result<()> {
    // Canonicalize so we can reliably compare event paths against the root.
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;

    let stats = scan(&root, opts)?;
    if !opts.quiet {
        println!("initial scan: {stats}");
    }

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(Duration::from_millis(debounce_ms), None, move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;
    debouncer
        .watcher()
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;
    debouncer.cache().add_root(&root, RecursiveMode::Recursive);

    if !opts.quiet {
        println!(
            "watching {} (debounce {debounce_ms}ms) — press Ctrl-C to stop",
            root.display()
        );
    }

    for res in rx {
        match res {
            Ok(events) => {
                // Map events to the set of directories that need reprocessing.
                let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
                for ev in &events {
                    for path in &ev.event.paths {
                        // Ignore our own manifest writes (and their temp files)
                        // to avoid a feedback loop, plus macOS AppleDouble
                        // sidecars that the OS spawns alongside our writes.
                        let ignore = path
                            .file_name()
                            .map(|n| {
                                let n = n.to_string_lossy();
                                is_manifest_file(&n)
                                    || (opts.skip_apple_double && is_apple_double(&n))
                            })
                            .unwrap_or(false);
                        if ignore {
                            continue;
                        }
                        if path.is_dir() {
                            dirs.insert(path.clone());
                        }
                        if let Some(parent) = path.parent() {
                            dirs.insert(parent.to_path_buf());
                        }
                    }
                }
                for d in dirs {
                    if !d.starts_with(&root) || !d.is_dir() {
                        continue;
                    }
                    match process_dir(&d, opts, &root) {
                        Ok((s, changed)) => {
                            if changed && !opts.quiet {
                                println!("updated {} — {s}", d.display());
                            }
                        }
                        Err(e) => {
                            if !opts.quiet {
                                eprintln!("hashit: error updating {}: {e:#}", d.display());
                            }
                        }
                    }
                }
            }
            Err(errors) => {
                if !opts.quiet {
                    for e in errors {
                        eprintln!("hashit: watch error: {e:#}");
                    }
                }
            }
        }
    }

    Ok(())
}
