use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Name of the per-directory manifest file.
pub const MANIFEST_NAME: &str = ".hashit";

/// Prefix of the transient files written during an atomic manifest save.
pub const MANIFEST_TMP_PREFIX: &str = ".hashit.tmp.";

/// Per-drive identity marker written at a scanned root (see `drive.rs`).
pub const DRIVE_MARKER_NAME: &str = ".hashit-drive";

/// On-disk format version. Bump when the schema changes incompatibly.
pub const MANIFEST_VERSION: u32 = 1;

/// True if `name` is a hashit-managed metadata file (manifest, its atomic-save
/// temp files, or the drive marker). These must never be hashed, inventoried,
/// indexed, or trigger watch reprocessing.
pub fn is_manifest_file(name: &str) -> bool {
    name == MANIFEST_NAME
        || name == DRIVE_MARKER_NAME
        || name.starts_with(MANIFEST_TMP_PREFIX)
}

/// One record per file directly contained in a directory.
///
/// `PartialEq`/`Eq` are used to detect whether a manifest actually changed so we
/// can avoid rewriting `.hashit` (and churning its mtime) on no-op scans.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub size: u64,
    /// Modified time as nanoseconds since the Unix epoch (0 if unavailable).
    pub mtime_ns: u64,
    pub hash: String,
    /// Hash algorithm used for `hash` (e.g. "blake3", "sha256").
    pub algo: String,
    /// Attribute flags such as "symlink", "executable", "hidden", "readonly".
    #[serde(default)]
    pub flags: Vec<String>,
    /// When the hash was last (re)computed, RFC3339.
    pub hashed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub updated_at: String,
    pub files: BTreeMap<String, FileEntry>,
}

impl Manifest {
    pub fn new(files: BTreeMap<String, FileEntry>) -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            updated_at: now_rfc3339(),
            files,
        }
    }

    /// Load the manifest from `dir/.hashit`. Returns `Ok(None)` when absent.
    pub fn load(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(MANIFEST_NAME);
        match fs::read(&path) {
            Ok(bytes) => {
                let m = serde_json::from_slice::<Manifest>(&bytes)
                    .with_context(|| format!("parsing manifest {}", path.display()))?;
                Ok(Some(m))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading manifest {}", path.display())),
        }
    }

    /// Write the manifest to `dir/.hashit` atomically (temp file + rename).
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(MANIFEST_NAME);
        // Include the thread id so two workers saving into the same directory
        // never collide on the temp filename (pid alone isn't unique per-writer).
        let tmp = dir.join(format!(
            "{MANIFEST_TMP_PREFIX}{}.{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let json = serde_json::to_vec_pretty(self).context("serializing manifest")?;
        // fsync the temp file before renaming so a crash or unclean unmount
        // (e.g. yanking an external drive) can't leave a truncated/zero-length
        // .hashit: the rename is atomic, but only durable if the data landed first.
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("flushing {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
        Ok(())
    }

    /// Remove `dir/.hashit` if present.
    pub fn delete(dir: &Path) -> Result<()> {
        let path = dir.join(MANIFEST_NAME);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

/// Current UTC time as an RFC3339 string with microsecond precision.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Convert nanoseconds-since-epoch into an RFC3339 string (empty on overflow).
pub fn ns_to_rfc3339(ns: u64) -> String {
    let secs = (ns / 1_000_000_000) as i64;
    let nanos = (ns % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
        .unwrap_or_default()
}

/// Modified time of `md` as nanoseconds since the Unix epoch (0 if unavailable).
pub fn mtime_ns(md: &fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Best-effort: set a file's modified time (e.g. to preserve it across a copy).
pub fn set_mtime(path: &Path, ns: u64) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_nanos(ns));
    }
}

/// Caches each directory's manifest so repeated lookups of a source file's
/// entry don't reload `.hashit` from disk.
#[derive(Default)]
pub struct ManifestCache {
    dirs: HashMap<PathBuf, BTreeMap<String, FileEntry>>,
}

impl ManifestCache {
    /// The manifest entry for `file`, if its directory has a manifest with it.
    pub fn entry_for(&mut self, file: &Path) -> Option<FileEntry> {
        let dir = file.parent()?.to_path_buf();
        let name = file.file_name()?.to_string_lossy().to_string();
        let files = self.dirs.entry(dir.clone()).or_insert_with(|| {
            Manifest::load(&dir)
                .ok()
                .flatten()
                .map(|m| m.files)
                .unwrap_or_default()
        });
        files.get(&name).cloned()
    }
}
