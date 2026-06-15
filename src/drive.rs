//! Drive identity and presence.
//!
//! Each scanned root carries a small `.hashit-drive` marker holding a generated
//! UUID and a human label. The UUID is the stable `drive_id` used to key
//! `locations` in the global index — it travels with the drive (works on exFAT,
//! unlike volume UUIDs) and lets the index flag content offline when the drive
//! is unplugged, or purge it when the drive is permanently detached.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::manifest::DRIVE_MARKER_NAME;
use crate::store::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveMarker {
    pub drive_id: String,
    #[serde(default)]
    pub label: String,
}

/// Path to a root's drive marker.
pub fn marker_path(root: &Path) -> PathBuf {
    root.join(DRIVE_MARKER_NAME)
}

/// Read a root's marker, or create one (new UUID + derived label) if absent or
/// unreadable. A corrupt marker is regenerated rather than aborting.
pub fn load_or_create(root: &Path) -> Result<DriveMarker> {
    let path = marker_path(root);
    if let Ok(bytes) = fs::read(&path) {
        if let Ok(m) = serde_json::from_slice::<DriveMarker>(&bytes) {
            if !m.drive_id.is_empty() {
                return Ok(m);
            }
        }
    }
    let marker = DriveMarker {
        drive_id: uuid::Uuid::new_v4().to_string(),
        label: derive_label(root),
    };
    write_marker(root, &marker)?;
    Ok(marker)
}

/// Atomically write the marker (temp + fsync + rename), mirroring manifest saves
/// so an unclean unmount can't leave a truncated drive id.
fn write_marker(root: &Path, marker: &DriveMarker) -> Result<()> {
    let path = marker_path(root);
    let tmp = root.join(format!(
        "{DRIVE_MARKER_NAME}.tmp.{}.{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let json = serde_json::to_vec_pretty(marker).context("serializing drive marker")?;
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// A friendly label: the macOS volume name (`/Volumes/<NAME>`) if the root is on
/// one, else the root directory's own name.
fn derive_label(root: &Path) -> String {
    let abs = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let comps: Vec<_> = abs.components().collect();
    // /Volumes/<NAME>/...  -> NAME
    for (i, c) in comps.iter().enumerate() {
        if c.as_os_str() == "Volumes" {
            if let Some(next) = comps.get(i + 1) {
                return next.as_os_str().to_string_lossy().to_string();
            }
        }
    }
    abs.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string())
}

/// Re-probe every registered drive and update its online flag: a drive is online
/// if its last known root still carries a matching marker.
pub fn refresh_presence(store: &Store) -> Result<()> {
    for d in store.list_drives()? {
        let online = probe(Path::new(&d.last_root), &d.drive_id);
        if online != d.online {
            store.set_drive_online(&d.drive_id, online)?;
        }
    }
    Ok(())
}

/// True if `root` carries a marker whose id matches `drive_id`.
fn probe(root: &Path, drive_id: &str) -> bool {
    if root.as_os_str().is_empty() {
        return false;
    }
    match fs::read(marker_path(root)) {
        Ok(bytes) => serde_json::from_slice::<DriveMarker>(&bytes)
            .map(|m| m.drive_id == drive_id)
            .unwrap_or(false),
        Err(_) => false,
    }
}
