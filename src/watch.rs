use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::manifest::is_manifest_file;
use crate::scan::{is_apple_double, process_dir, scan, ScanOptions, ScanStats};

/// Run a full scan, then watch every `root` recursively and keep `.hashit`
/// manifests updated as files are added, changed, or removed. Blocks until
/// interrupted.
///
/// `on_dir(root, dir)` is invoked after each directory is successfully
/// reprocessed, letting callers (e.g. the metadata indexer) react to changes.
/// Pass `|_, _| {}` when no hook is needed.
pub fn watch(
    roots: &[PathBuf],
    opts: &ScanOptions,
    debounce_ms: u64,
    mut on_dir: impl FnMut(&Path, &Path),
) -> Result<()> {
    // Canonicalize so we can reliably compare event paths against the roots.
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| {
            r.canonicalize()
                .with_context(|| format!("resolving {}", r.display()))
        })
        .collect::<Result<_>>()?;

    let mut stats = ScanStats::default();
    for root in &roots {
        stats = stats.merge(scan(root, opts)?);
    }
    if !opts.quiet {
        println!("initial scan: {stats}");
    }

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(Duration::from_millis(debounce_ms), None, move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;
    for root in &roots {
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.display()))?;
        debouncer.cache().add_root(root, RecursiveMode::Recursive);
    }

    if !opts.quiet {
        let list: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        println!(
            "watching {} (debounce {debounce_ms}ms) — press Ctrl-C to stop",
            list.join(", ")
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
                    // Attribute each directory to the watched root that contains
                    // it (longest match wins for nested roots); skip stray events.
                    let Some(root) = roots
                        .iter()
                        .filter(|r| d.starts_with(r))
                        .max_by_key(|r| r.components().count())
                    else {
                        continue;
                    };
                    if !d.is_dir() {
                        continue;
                    }
                    match process_dir(&d, opts, root) {
                        Ok((s, changed)) => {
                            if changed && !opts.quiet {
                                println!("updated {} — {s}", d.display());
                            }
                            on_dir(root, &d);
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
