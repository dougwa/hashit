use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use console::{Key, Term};

use crate::inventory::build_inventory;
use crate::scan::{process_dir, scan, ScanOptions, ScanStats, DEDUP_SUFFIX};

pub struct DedupOptions {
    /// When true, prompt per duplicate set; otherwise auto-select.
    pub interactive: bool,
    /// Leave a `<file>.dedup` pointer for each removed duplicate.
    pub write_links: bool,
    /// Report intended actions without deleting or writing anything.
    pub dry_run: bool,
}

/// One candidate file within a duplicate set, with precomputed sort keys.
struct Candidate {
    abs: PathBuf,
    /// Path relative to its root; drives the ranking heuristic.
    rel: String,
    /// What to show the user: the root-relative path for a single root, or the
    /// absolute path when deduping across multiple roots (so it's unambiguous).
    display: String,
    size: u64,
    hidden: bool,
    slashes: usize,
}

enum Choice {
    Keep(usize),
    Skip,
    Auto,
}

/// True if any path segment starts with '.'.
fn rel_is_hidden(rel: &str) -> bool {
    rel.split('/').any(|seg| seg.starts_with('.'))
}

/// Order candidates by the auto-keep ranking: non-hidden first, then fewest
/// path separators, then alphabetical. After sorting, index 0 is the keeper.
fn rank(cands: &mut [Candidate]) {
    cands.sort_by(|a, b| {
        a.hidden
            .cmp(&b.hidden)
            .then(a.slashes.cmp(&b.slashes))
            .then(a.rel.cmp(&b.rel))
    });
}

/// Compute the path of `to` relative to directory `from_dir` (both absolute and
/// sharing a common root), e.g. `../../a/b/x.jpg`.
fn relative_to(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from_dir.components().collect();
    let to_c: Vec<Component> = to.components().collect();
    let mut i = 0;
    while i < from.len() && i < to_c.len() && from[i] == to_c[i] {
        i += 1;
    }
    let mut out = PathBuf::new();
    for _ in i..from.len() {
        out.push("..");
    }
    for c in &to_c[i..] {
        out.push(c.as_os_str());
    }
    out
}

/// Path of the pointer file for a removed duplicate: `<removed path>.dedup`.
fn pointer_path(removed: &Path) -> PathBuf {
    let mut os = removed.as_os_str().to_os_string();
    os.push(DEDUP_SUFFIX);
    PathBuf::from(os)
}

fn prompt_choice(n: usize) -> Result<Choice> {
    // Single-keypress input needs a real terminal on both ends; otherwise fall
    // back to line input (handles pipes, redirects, and non-interactive stdin).
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        prompt_choice_key(n)
    } else {
        prompt_choice_line(n)
    }
}

/// Read a choice one keypress at a time, no ENTER required. A digit is accepted
/// the moment it can't be the prefix of a larger in-range index (e.g. with 8
/// candidates "3" commits immediately; with 12 candidates "1" waits, since "10"
/// is still reachable, and ENTER then commits the bare "1").
fn prompt_choice_key(n: usize) -> Result<Choice> {
    let term = Term::stdout();
    loop {
        term.write_str(&format!("Keep which? [1-{n}, (s)kip, (a)uto]: "))?;
        let mut digits = String::new();
        let choice = loop {
            match term.read_key()? {
                Key::Char('s' | 'S') if digits.is_empty() => break Some(Choice::Skip),
                Key::Char('a' | 'A') if digits.is_empty() => break Some(Choice::Auto),
                Key::Char(c @ '0'..='9') => {
                    let v: usize = format!("{digits}{c}").parse().unwrap_or(usize::MAX);
                    // Reject leading zero and anything already past the top index.
                    if v == 0 || v > n {
                        continue;
                    }
                    digits.push(c);
                    term.write_str(&c.to_string())?;
                    // No longer-reachable index has these digits as a prefix.
                    if v.saturating_mul(10) > n {
                        break Some(Choice::Keep(v - 1));
                    }
                }
                Key::Backspace => {
                    if digits.pop().is_some() {
                        term.clear_chars(1)?;
                    }
                }
                Key::Enter => {
                    if let Ok(v) = digits.parse::<usize>() {
                        if (1..=n).contains(&v) {
                            break Some(Choice::Keep(v - 1));
                        }
                    }
                    // Bare ENTER or out-of-range: ignore and keep reading.
                }
                _ => {}
            }
        };
        term.write_line("")?;
        if let Some(c) = choice {
            return Ok(c);
        }
    }
}

/// Line-buffered fallback for non-interactive use.
fn prompt_choice_line(n: usize) -> Result<Choice> {
    let mut input = String::new();
    loop {
        print!("Keep which? [1-{n}, (s)kip, (a)uto]: ");
        io::stdout().flush()?;
        input.clear();
        if io::stdin().read_line(&mut input)? == 0 {
            // EOF (e.g. non-interactive stdin): skip rather than delete blindly.
            println!();
            return Ok(Choice::Skip);
        }
        match input.trim() {
            "s" | "S" | "skip" => return Ok(Choice::Skip),
            "a" | "A" | "auto" => return Ok(Choice::Auto),
            t => match t.parse::<usize>() {
                Ok(i) if (1..=n).contains(&i) => return Ok(Choice::Keep(i - 1)),
                _ => eprintln!("  invalid choice: '{t}'"),
            },
        }
    }
}

/// Delete one duplicate and (optionally) leave a pointer to the kept file.
fn remove_one(removed: &Path, kept: &Path, opts: &DedupOptions) -> Result<()> {
    if opts.dry_run {
        return Ok(());
    }
    fs::remove_file(removed).with_context(|| format!("removing {}", removed.display()))?;
    if opts.write_links {
        let from_dir = removed.parent().unwrap_or(Path::new("."));
        let target = relative_to(from_dir, kept);
        let ptr = pointer_path(removed);
        fs::write(&ptr, format!("{}\n", target.to_string_lossy()))
            .with_context(|| format!("writing pointer {}", ptr.display()))?;
    }
    Ok(())
}

pub fn dedup(roots: &[PathBuf], scan_opts: &ScanOptions, dd: &DedupOptions) -> Result<()> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| {
            r.canonicalize()
                .with_context(|| format!("resolving {}", r.display()))
        })
        .collect::<Result<_>>()?;
    let multi = roots.len() > 1;

    // 1) Bring every manifest up to date before reading hashes.
    let mut stats = ScanStats::default();
    for root in &roots {
        stats = stats.merge(scan(root, scan_opts)?);
    }
    if !scan_opts.quiet {
        println!("scan: {stats}");
    }

    // 2) Group files by (algo, hash) across every root. Different algos never
    //    share a set. Deduping multiple roots finds duplicates between them.
    let mut groups: BTreeMap<(String, String), Vec<Candidate>> = BTreeMap::new();
    for root in &roots {
        for r in build_inventory(root)? {
            let abs = root.join(&r.path);
            let hidden = rel_is_hidden(&r.path);
            let slashes = r.path.matches('/').count();
            let display = if multi {
                abs.display().to_string()
            } else {
                r.path.clone()
            };
            groups.entry((r.algo, r.hash)).or_default().push(Candidate {
                abs,
                rel: r.path,
                display,
                size: r.size,
                hidden,
                slashes,
            });
        }
    }

    let mut auto = !dd.interactive;
    let mut affected: BTreeSet<PathBuf> = BTreeSet::new();
    let (mut dup_sets, mut kept, mut removed, mut skipped) = (0usize, 0usize, 0usize, 0usize);

    for ((algo, hash), mut cands) in groups {
        if cands.len() < 2 {
            continue;
        }
        dup_sets += 1;
        rank(&mut cands);

        let keep_idx = if auto {
            Some(0)
        } else {
            println!(
                "\nDuplicate set [{algo}:{}] — {} copies, {} bytes each:",
                &hash[..hash.len().min(12)],
                cands.len(),
                cands[0].size
            );
            for (i, c) in cands.iter().enumerate() {
                let marker = if i == 0 { " (auto-keep)" } else { "" };
                println!("  {}) {}{marker}", i + 1, c.display);
            }
            match prompt_choice(cands.len())? {
                Choice::Keep(i) => Some(i),
                Choice::Skip => {
                    skipped += 1;
                    None
                }
                Choice::Auto => {
                    auto = true;
                    Some(0)
                }
            }
        };

        let Some(keep) = keep_idx else {
            continue;
        };
        kept += 1;
        let kept_abs = cands[keep].abs.clone();
        let kept_display = cands[keep].display.clone();
        if !scan_opts.quiet {
            let verb = if dd.dry_run { "would keep" } else { "keep" };
            println!("  {verb} {kept_display}");
        }
        for (i, c) in cands.iter().enumerate() {
            if i == keep {
                continue;
            }
            remove_one(&c.abs, &kept_abs, dd)?;
            removed += 1;
            if let Some(d) = c.abs.parent() {
                affected.insert(d.to_path_buf());
            }
            if !scan_opts.quiet {
                let verb = if dd.dry_run { "would remove" } else { "removed" };
                println!("  {verb} {}", c.display);
            }
        }
    }

    // 3) Refresh the manifests of directories we changed (silently).
    if !dd.dry_run {
        let mut refresh = scan_opts.clone();
        refresh.quiet = true;
        refresh.verbose = false;
        refresh.status = false;
        for d in &affected {
            // Reconcile against the root that owns the directory (longest match).
            if let Some(root) = roots
                .iter()
                .filter(|r| d.starts_with(r))
                .max_by_key(|r| r.components().count())
            {
                let _ = process_dir(d, &refresh, root);
            }
        }
    }

    let prefix = if dd.dry_run { "dry-run: " } else { "" };
    println!(
        "{prefix}{dup_sets} duplicate sets — kept {kept}, removed {removed}, skipped {skipped}",
    );
    Ok(())
}
