use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

use crate::inventory::build_inventory;
use crate::scan::{scan, ScanOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum DiffFormat {
    /// Two sections listing the files unique to each side (default).
    Grouped,
    /// Single stream: '-' only in path1, '+' only in path2.
    Unified,
    /// Structured {only_in_path1, only_in_path2, common} for scripting.
    Json,
    /// Counts only, no per-file detail.
    Summary,
}

pub struct DiffOptions {
    pub format: DiffFormat,
    pub show_common: bool,
    /// Scan both paths before comparing (refreshes their .hashit manifests).
    pub scan_first: bool,
}

/// (algo, hash) -> sorted list of relative file paths carrying that hash.
pub(crate) type HashIndex = BTreeMap<(String, String), Vec<String>>;

/// One side's unique entries: references into a HashIndex.
type Entries<'a> = Vec<(&'a (String, String), &'a Vec<String>)>;

pub(crate) fn collect(root: &Path) -> Result<HashIndex> {
    let mut idx: HashIndex = BTreeMap::new();
    for r in build_inventory(root)? {
        idx.entry((r.algo, r.hash)).or_default().push(r.path);
    }
    for files in idx.values_mut() {
        files.sort();
    }
    Ok(idx)
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn hashes_word(n: usize) -> &'static str {
    if n == 1 {
        "hash"
    } else {
        "hashes"
    }
}

fn files_word(n: usize) -> &'static str {
    if n == 1 {
        "file"
    } else {
        "files"
    }
}

fn file_total(entries: &Entries) -> usize {
    entries.iter().map(|e| e.1.len()).sum()
}

#[derive(Serialize)]
struct GroupOut {
    algo: String,
    hash: String,
    files: Vec<String>,
}

#[derive(Serialize)]
struct CommonOut {
    algo: String,
    hash: String,
    path1_files: Vec<String>,
    path2_files: Vec<String>,
}

#[derive(Serialize)]
struct SummaryOut {
    path1_only_hashes: usize,
    path1_only_files: usize,
    path2_only_hashes: usize,
    path2_only_files: usize,
    common_hashes: usize,
}

#[derive(Serialize)]
struct DiffOut {
    path1: String,
    path2: String,
    only_in_path1: Vec<GroupOut>,
    only_in_path2: Vec<GroupOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    common: Option<Vec<CommonOut>>,
    summary: SummaryOut,
}

fn to_groups(entries: &Entries) -> Vec<GroupOut> {
    entries
        .iter()
        .map(|e| GroupOut {
            algo: e.0 .0.clone(),
            hash: e.0 .1.clone(),
            files: e.1.clone(),
        })
        .collect()
}

/// Print one side's unique files, sorted by path.
fn print_side(path: &Path, entries: &Entries) {
    let hashes = entries.len();
    let files = file_total(entries);
    println!(
        "Only in {}  ({hashes} {}, {files} {}):",
        path.display(),
        hashes_word(hashes),
        files_word(files),
    );
    let mut lines: Vec<(&str, &str)> = Vec::new();
    for e in entries {
        for f in e.1.iter() {
            lines.push((f.as_str(), e.0 .1.as_str()));
        }
    }
    lines.sort_by(|a, b| a.0.cmp(b.0));
    for (f, h) in lines {
        println!("  {f}   [{}]", short(h));
    }
}

pub fn diff(p1: &Path, p2: &Path, scan_opts: &ScanOptions, dd: &DiffOptions) -> Result<()> {
    let r1 = p1
        .canonicalize()
        .with_context(|| format!("resolving {}", p1.display()))?;
    let r2 = p2
        .canonicalize()
        .with_context(|| format!("resolving {}", p2.display()))?;

    // Scan summaries go to stderr so stdout stays clean for JSON/piping.
    if dd.scan_first {
        let s1 = scan(&r1, scan_opts)?;
        let s2 = scan(&r2, scan_opts)?;
        if !scan_opts.quiet {
            eprintln!("scan {}: {s1}", r1.display());
            eprintln!("scan {}: {s2}", r2.display());
        }
    }

    let m1 = collect(&r1)?;
    let m2 = collect(&r2)?;

    // Warn if the two trees use entirely different hash algorithms.
    let algos1: BTreeSet<&str> = m1.keys().map(|(a, _)| a.as_str()).collect();
    let algos2: BTreeSet<&str> = m2.keys().map(|(a, _)| a.as_str()).collect();
    if !m1.is_empty() && !m2.is_empty() && algos1.is_disjoint(&algos2) {
        eprintln!(
            "warning: paths hashed with different algorithms ({algos1:?} vs {algos2:?}); \
             everything will appear different — re-run with a matching --hash (and without --no-scan)."
        );
    }

    let only1: Entries = m1.iter().filter(|e| !m2.contains_key(e.0)).collect();
    let only2: Entries = m2.iter().filter(|e| !m1.contains_key(e.0)).collect();
    let common: Entries = m1.iter().filter(|e| m2.contains_key(e.0)).collect();

    let (h1, f1) = (only1.len(), file_total(&only1));
    let (h2, f2) = (only2.len(), file_total(&only2));
    let hc = common.len();

    match dd.format {
        DiffFormat::Grouped => {
            print_side(&r1, &only1);
            println!();
            print_side(&r2, &only2);
            if dd.show_common {
                println!("\nIn both  ({hc} {}):", hashes_word(hc));
                for e in &common {
                    let k = e.0;
                    let f2files = m2.get(k).map(|v| v.as_slice()).unwrap_or(&[]);
                    let rep1 = e.1.first().map(String::as_str).unwrap_or("");
                    let rep2 = f2files.first().map(String::as_str).unwrap_or("");
                    println!("  [{}]  {rep1}  <=>  {rep2}", short(&k.1));
                }
            }
            println!("\nsummary: <-only {h1}  >-only {h2}  common {hc}");
        }
        DiffFormat::Unified => {
            let mut minus: Vec<(&str, &str)> = Vec::new();
            for e in &only1 {
                for f in e.1 {
                    minus.push((f.as_str(), e.0 .1.as_str()));
                }
            }
            minus.sort_by(|a, b| a.0.cmp(b.0));
            for (f, h) in &minus {
                println!("- {f}   {}", short(h));
            }
            let mut plus: Vec<(&str, &str)> = Vec::new();
            for e in &only2 {
                for f in e.1 {
                    plus.push((f.as_str(), e.0 .1.as_str()));
                }
            }
            plus.sort_by(|a, b| a.0.cmp(b.0));
            for (f, h) in &plus {
                println!("+ {f}   {}", short(h));
            }
            if dd.show_common {
                for e in &common {
                    let n2 = m2.get(e.0).map(|v| v.len()).unwrap_or(0);
                    println!("= {}   {}|{n2}", short(&e.0 .1), e.1.len());
                }
            }
            println!("\nsummary: {f1} removed, {f2} added, {hc} common");
        }
        DiffFormat::Json => {
            let common_out = dd.show_common.then(|| {
                common
                    .iter()
                    .map(|e| CommonOut {
                        algo: e.0 .0.clone(),
                        hash: e.0 .1.clone(),
                        path1_files: e.1.clone(),
                        path2_files: m2.get(e.0).cloned().unwrap_or_default(),
                    })
                    .collect()
            });
            let out = DiffOut {
                path1: r1.display().to_string(),
                path2: r2.display().to_string(),
                only_in_path1: to_groups(&only1),
                only_in_path2: to_groups(&only2),
                common: common_out,
                summary: SummaryOut {
                    path1_only_hashes: h1,
                    path1_only_files: f1,
                    path2_only_hashes: h2,
                    path2_only_files: f2,
                    common_hashes: hc,
                },
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&out).context("serializing diff")?
            );
        }
        DiffFormat::Summary => {
            println!(
                "path1 ({}):  only {h1} {} / {f1} {}",
                r1.display(),
                hashes_word(h1),
                files_word(f1),
            );
            println!(
                "path2 ({}):  only {h2} {} / {f2} {}",
                r2.display(),
                hashes_word(h2),
                files_word(f2),
            );
            println!("common:      {hc} {}", hashes_word(hc));
        }
    }

    Ok(())
}
