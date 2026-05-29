use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use walkdir::WalkDir;

use crate::manifest::{ns_to_rfc3339, Manifest, MANIFEST_NAME};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Json,
    Csv,
}

#[derive(Debug, Serialize)]
pub struct InventoryRecord {
    /// Path relative to the scan root, using forward slashes.
    pub path: String,
    pub hash: String,
    pub algo: String,
    /// Flags joined with '|' (CSV-friendly).
    pub flags: String,
    pub size: u64,
    pub mtime_ns: u64,
    pub mtime: String,
    pub hashed_at: String,
}

/// Normalize a path to forward-slash form for portable output.
fn to_slash(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        if let Component::Normal(s) = c {
            parts.push(s.to_string_lossy().to_string());
        }
    }
    parts.join("/")
}

/// Walk every `.hashit` under `root` and aggregate entries into a sorted report.
pub fn build_inventory(root: &Path) -> Result<Vec<InventoryRecord>> {
    let mut records: Vec<InventoryRecord> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() || entry.file_name() != MANIFEST_NAME {
            continue;
        }
        let dir = entry.path().parent().unwrap_or(root);
        let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new(""));
        let manifest = match Manifest::load(dir)? {
            Some(m) => m,
            None => continue,
        };
        for (name, fe) in manifest.files {
            let mut full: PathBuf = rel_dir.to_path_buf();
            full.push(&name);
            records.push(InventoryRecord {
                path: to_slash(&full),
                hash: fe.hash,
                algo: fe.algo,
                flags: fe.flags.join("|"),
                size: fe.size,
                mtime_ns: fe.mtime_ns,
                mtime: ns_to_rfc3339(fe.mtime_ns),
                hashed_at: fe.hashed_at,
            });
        }
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(records)
}

/// Render the inventory to stdout or a file in the requested format.
pub fn write_inventory(
    records: &[InventoryRecord],
    format: Format,
    out: Option<&Path>,
) -> Result<()> {
    let mut sink: Box<dyn Write> = match out {
        Some(p) => Box::new(File::create(p).with_context(|| format!("creating {}", p.display()))?),
        None => Box::new(io::stdout().lock()),
    };
    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(records).context("serializing inventory")?;
            sink.write_all(json.as_bytes())?;
            sink.write_all(b"\n")?;
        }
        Format::Csv => {
            let mut wtr = csv::Writer::from_writer(&mut sink);
            for r in records {
                wtr.serialize(r).context("writing csv row")?;
            }
            wtr.flush()?;
        }
    }
    Ok(())
}
