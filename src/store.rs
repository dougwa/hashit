//! The global, drive-aware metadata index.
//!
//! Lives at `~/.hashit/index.db` (SQLite) with a sibling `~/.hashit/cache/` for
//! thumbnails. The index is **derived and rebuildable** from the per-directory
//! `.hashit` manifests plus the extractors, so corruption is always recoverable
//! by re-indexing. Metadata and thumbnails are stored **once per content hash**;
//! `locations` records every (drive, path) where a hash is found.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::manifest::now_rfc3339;

/// Bump when the schema changes incompatibly.
const SCHEMA_VERSION: i64 = 1;

/// The reserved tag used to mark favorites.
pub const FAVORITE_TAG: &str = "favorite";

/// One flattened metadata tag (EAV row).
#[derive(Debug, Serialize)]
pub struct MetaTag {
    pub group: String,
    pub key: String,
    pub value: String,
}

/// Everything extracted for a single content hash.
pub struct ContentMeta {
    pub size: u64,
    pub file_type: Option<String>,
    pub ext: Option<String>,
    pub extractor_version: i64,
    pub has_thumb: bool,
    pub tags: Vec<MetaTag>,
}

/// A registered drive.
#[derive(Debug, Serialize)]
pub struct DriveRow {
    pub drive_id: String,
    pub label: String,
    pub last_root: String,
    pub first_seen: String,
    pub last_seen: String,
    pub online: bool,
    pub detached: bool,
    /// Number of (non-detached) location rows on this drive.
    pub files: i64,
}

/// Filters for a paginated query against the index.
#[derive(Default)]
pub struct QueryFilter {
    pub file_type: Option<String>,
    pub ext: Option<String>,
    pub hash_prefix: Option<String>,
    pub drive_id: Option<String>,
    pub offline_only: bool,
    pub key: Option<String>,
    pub value: Option<String>,
    /// Only content carrying this user tag.
    pub tag: Option<String>,
    /// Only favorites (sugar for `tag = FAVORITE_TAG`).
    pub favorite: bool,
    pub limit: i64,
    pub offset: i64,
}

/// One result row: a content hash plus where/how it appears.
#[derive(Debug, Serialize)]
pub struct QueryRow {
    pub algo: String,
    pub hash: String,
    pub file_type: Option<String>,
    pub ext: Option<String>,
    pub size: i64,
    /// Number of locations (copies) recorded for this hash.
    pub locations: i64,
    /// True if any location is on a currently-online drive.
    pub online: bool,
    pub has_thumb: bool,
    /// Comma-separated drive ids holding this hash.
    pub drives: String,
    /// A representative path (lexicographically smallest).
    pub sample_path: String,
    /// Comma-separated user tags on this hash.
    pub tags: String,
    /// Link group id, if this hash is linked to others.
    pub link_group: Option<String>,
}

/// One entry in a logical directory listing: a file or a subdirectory.
#[cfg(feature = "serve")]
#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    /// "file" or "dir".
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_thumb: Option<bool>,
    /// For directories: number of files beneath it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<i64>,
}

#[cfg(feature = "serve")]
impl DirEntry {
    fn dir(name: String, file_count: i64) -> DirEntry {
        DirEntry {
            name,
            kind: "dir".to_string(),
            algo: None,
            hash: None,
            size: None,
            file_type: None,
            ext: None,
            has_thumb: None,
            file_count: Some(file_count),
        }
    }
}

/// A location joined with its drive's root + online state, for resolving an
/// absolute path.
#[cfg(feature = "serve")]
pub struct ResolvedLoc {
    pub drive_id: String,
    pub path: String,
    pub last_root: String,
    pub online: bool,
}

/// One place a content hash is found.
#[cfg(feature = "serve")]
#[derive(Debug, Serialize)]
pub struct LocationRow {
    pub drive_id: String,
    pub path: String,
    pub online: bool,
}

/// Full detail for one content hash.
#[cfg(feature = "serve")]
#[derive(Debug, Serialize)]
pub struct ContentDetail {
    pub algo: String,
    pub hash: String,
    pub size: i64,
    pub file_type: Option<String>,
    pub ext: Option<String>,
    pub extracted_at: String,
    pub has_thumb: bool,
    pub metadata: Vec<MetaTag>,
    pub locations: Vec<LocationRow>,
    pub tags: Vec<String>,
    /// Other hashes linked to this one.
    pub links: Vec<String>,
}

/// The exclusive upper bound for a path-prefix range scan: `prefix` with its
/// last byte incremented (e.g. "a/" -> "a0"), so `path >= prefix AND path < hi`
/// selects exactly the subtree without LIKE-metacharacter pitfalls.
#[cfg(feature = "serve")]
fn prefix_upper(prefix: &str) -> String {
    let mut bytes = prefix.as_bytes().to_vec();
    if let Some(last) = bytes.last_mut() {
        *last += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub struct Store {
    conn: Connection,
    cache_dir: PathBuf,
}

/// `~/.hashit` (or `$HASHIT_HOME` if set, mainly for tests).
pub fn home_dir() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("HASHIT_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".hashit"))
}

impl Store {
    /// Open (creating if needed) the index at `~/.hashit/index.db`.
    pub fn open_default() -> Result<Store> {
        let dir = home_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Store::open_at(&dir)
    }

    /// Open the index rooted at `dir` (`dir/index.db`, `dir/cache/`).
    pub fn open_at(dir: &Path) -> Result<Store> {
        let cache_dir = dir.join("cache");
        fs::create_dir_all(&cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
        let conn = Connection::open(dir.join("index.db")).context("opening index.db")?;
        // WAL keeps reads concurrent with the single writer and survives crashes.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store { conn, cache_dir };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT);

            CREATE TABLE IF NOT EXISTS drives (
                drive_id   TEXT PRIMARY KEY,
                label      TEXT NOT NULL DEFAULT '',
                last_root  TEXT NOT NULL DEFAULT '',
                first_seen TEXT NOT NULL,
                last_seen  TEXT NOT NULL,
                online     INTEGER NOT NULL DEFAULT 1,
                detached   INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS content (
                algo              TEXT NOT NULL,
                hash              TEXT NOT NULL,
                size              INTEGER NOT NULL,
                file_type         TEXT,
                ext               TEXT,
                extracted_at      TEXT NOT NULL,
                extractor_version INTEGER NOT NULL DEFAULT 0,
                has_thumb         INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (algo, hash)
            );

            CREATE TABLE IF NOT EXISTS metadata (
                algo  TEXT NOT NULL,
                hash  TEXT NOT NULL,
                grp   TEXT NOT NULL DEFAULT '',
                key   TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (algo, hash, key)
            );

            CREATE TABLE IF NOT EXISTS locations (
                algo     TEXT NOT NULL,
                hash     TEXT NOT NULL,
                drive_id TEXT NOT NULL,
                path     TEXT NOT NULL,
                mtime_ns INTEGER NOT NULL DEFAULT 0,
                seen_at  TEXT NOT NULL,
                PRIMARY KEY (algo, hash, drive_id, path)
            );

            CREATE TABLE IF NOT EXISTS tags (
                algo     TEXT NOT NULL,
                hash     TEXT NOT NULL,
                tag      TEXT NOT NULL,
                added_at TEXT NOT NULL,
                PRIMARY KEY (algo, hash, tag)
            );

            CREATE TABLE IF NOT EXISTS link_members (
                group_id TEXT NOT NULL,
                algo     TEXT NOT NULL,
                hash     TEXT NOT NULL,
                added_at TEXT NOT NULL,
                PRIMARY KEY (algo, hash)
            );

            CREATE INDEX IF NOT EXISTS idx_loc_drive ON locations (drive_id);
            CREATE INDEX IF NOT EXISTS idx_loc_hash  ON locations (algo, hash);
            CREATE INDEX IF NOT EXISTS idx_loc_drive_path ON locations (drive_id, path);
            CREATE INDEX IF NOT EXISTS idx_content_type ON content (file_type);
            CREATE INDEX IF NOT EXISTS idx_content_ext  ON content (ext);
            CREATE INDEX IF NOT EXISTS idx_meta_kv ON metadata (key, value);
            CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags (tag);
            CREATE INDEX IF NOT EXISTS idx_link_group ON link_members (group_id);
            "#,
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO schema_meta (key, value) VALUES ('version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    // -- cache paths --------------------------------------------------------

    /// Where a hash's thumbnail lives: `cache/<algo>/<hh>/<hash>.jpg`.
    pub fn thumb_path(&self, algo: &str, hash: &str) -> PathBuf {
        let hh = &hash[..hash.len().min(2)];
        self.cache_dir.join(algo).join(hh).join(format!("{hash}.jpg"))
    }

    // -- drives -------------------------------------------------------------

    /// Record a drive as seen now (online) at `root`. Preserves `detached`.
    pub fn upsert_drive(&self, drive_id: &str, label: &str, root: &Path) -> Result<()> {
        let now = now_rfc3339();
        let root = root.to_string_lossy();
        self.conn.execute(
            r#"
            INSERT INTO drives (drive_id, label, last_root, first_seen, last_seen, online, detached)
            VALUES (?1, ?2, ?3, ?4, ?4, 1, 0)
            ON CONFLICT(drive_id) DO UPDATE SET
                label = excluded.label,
                last_root = excluded.last_root,
                last_seen = excluded.last_seen,
                online = 1
            "#,
            params![drive_id, label, root, now],
        )?;
        Ok(())
    }

    /// All non-detached drives, with a live file count.
    pub fn list_drives(&self) -> Result<Vec<DriveRow>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT d.drive_id, d.label, d.last_root, d.first_seen, d.last_seen,
                   d.online, d.detached,
                   (SELECT COUNT(*) FROM locations l WHERE l.drive_id = d.drive_id)
            FROM drives d
            WHERE d.detached = 0
            ORDER BY d.label, d.drive_id
            "#,
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DriveRow {
                    drive_id: r.get(0)?,
                    label: r.get(1)?,
                    last_root: r.get(2)?,
                    first_seen: r.get(3)?,
                    last_seen: r.get(4)?,
                    online: r.get::<_, i64>(5)? != 0,
                    detached: r.get::<_, i64>(6)? != 0,
                    files: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Set a drive's online flag (driven by a presence probe in `drive.rs`).
    pub fn set_drive_online(&self, drive_id: &str, online: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE drives SET online = ?2 WHERE drive_id = ?1",
            params![drive_id, online as i64],
        )?;
        Ok(())
    }

    pub fn relabel_drive(&self, drive_id: &str, label: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE drives SET label = ?2 WHERE drive_id = ?1 AND detached = 0",
            params![drive_id, label],
        )?;
        Ok(n > 0)
    }

    /// Permanently detach a drive: drop its locations, then prune any content/
    /// metadata that no longer has a location. Deletes orphaned thumbnails.
    /// Returns the number of locations removed.
    pub fn detach_drive(&mut self, drive_id: &str) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let removed = tx.execute(
            "DELETE FROM locations WHERE drive_id = ?1",
            params![drive_id],
        )? as i64;
        tx.execute(
            "UPDATE drives SET detached = 1, online = 0 WHERE drive_id = ?1",
            params![drive_id],
        )?;
        // Collect orphaned content (no remaining location) to delete thumbnails.
        let orphans: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                r#"
                SELECT c.algo, c.hash FROM content c
                WHERE NOT EXISTS (
                    SELECT 1 FROM locations l WHERE l.algo = c.algo AND l.hash = c.hash
                )
                "#,
            )?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        tx.execute_batch(
            r#"
            DELETE FROM metadata WHERE NOT EXISTS (
                SELECT 1 FROM locations l WHERE l.algo = metadata.algo AND l.hash = metadata.hash
            );
            DELETE FROM content WHERE NOT EXISTS (
                SELECT 1 FROM locations l WHERE l.algo = content.algo AND l.hash = content.hash
            );
            "#,
        )?;
        tx.commit()?;
        for (algo, hash) in orphans {
            let _ = fs::remove_file(self.thumb_path(&algo, &hash));
        }
        Ok(removed)
    }

    // -- content / locations ------------------------------------------------

    /// True if metadata has already been extracted for this content hash.
    pub fn has_content(&self, algo: &str, hash: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM content WHERE algo = ?1 AND hash = ?2",
                params![algo, hash],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Insert content + its metadata rows (only for a hash not yet present).
    pub fn insert_content(&mut self, algo: &str, hash: &str, m: &ContentMeta) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            r#"
            INSERT OR REPLACE INTO content
                (algo, hash, size, file_type, ext, extracted_at, extractor_version, has_thumb)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                algo,
                hash,
                m.size as i64,
                m.file_type,
                m.ext,
                now_rfc3339(),
                m.extractor_version,
                m.has_thumb as i64,
            ],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO metadata (algo, hash, grp, key, value) VALUES (?1,?2,?3,?4,?5)",
            )?;
            for t in &m.tags {
                stmt.execute(params![algo, hash, t.group, t.key, t.value])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget a hash's content + metadata rows (not its locations), so a
    /// reindex can rewrite them cleanly without leaving dropped tag keys behind.
    pub fn forget_content(&mut self, algo: &str, hash: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM metadata WHERE algo = ?1 AND hash = ?2",
            params![algo, hash],
        )?;
        tx.execute(
            "DELETE FROM content WHERE algo = ?1 AND hash = ?2",
            params![algo, hash],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Mark `has_thumb` for an existing content row (e.g. on-demand thumbnail).
    pub fn set_has_thumb(&self, algo: &str, hash: &str, has: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE content SET has_thumb = ?3 WHERE algo = ?1 AND hash = ?2",
            params![algo, hash, has as i64],
        )?;
        Ok(())
    }

    /// Record (or refresh) a location for a hash.
    pub fn upsert_location(
        &self,
        algo: &str,
        hash: &str,
        drive_id: &str,
        path: &str,
        mtime_ns: u64,
        seen_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO locations (algo, hash, drive_id, path, mtime_ns, seen_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(algo, hash, drive_id, path) DO UPDATE SET
                mtime_ns = excluded.mtime_ns,
                seen_at  = excluded.seen_at
            "#,
            params![algo, hash, drive_id, path, mtime_ns as i64, seen_at],
        )?;
        Ok(())
    }

    /// Remove a single (drive, path) location for a hash (used after the API
    /// dedup endpoint deletes a duplicate file).
    #[cfg(feature = "serve")]
    pub fn remove_location(
        &self,
        algo: &str,
        hash: &str,
        drive_id: &str,
        path: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM locations WHERE algo=?1 AND hash=?2 AND drive_id=?3 AND path=?4",
            params![algo, hash, drive_id, path],
        )?;
        Ok(())
    }

    /// Every location of a hash, joined with its drive's root and online state,
    /// so callers can build absolute paths and skip offline copies.
    #[cfg(feature = "serve")]
    pub fn locations_for(&self, algo: &str, hash: &str) -> Result<Vec<ResolvedLoc>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT l.drive_id, l.path, dr.last_root, dr.online
            FROM locations l JOIN drives dr ON dr.drive_id = l.drive_id
            WHERE l.algo=?1 AND l.hash=?2 AND dr.detached=0
            ORDER BY l.drive_id, l.path
            "#,
        )?;
        let rows = stmt
            .query_map(params![algo, hash], |r| {
                Ok(ResolvedLoc {
                    drive_id: r.get(0)?,
                    path: r.get(1)?,
                    last_root: r.get(2)?,
                    online: r.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop a drive's locations not refreshed since `since` (files that vanished
    /// from the tree during the latest index run).
    pub fn prune_stale_locations(&self, drive_id: &str, since: &str) -> Result<i64> {
        let n = self.conn.execute(
            "DELETE FROM locations WHERE drive_id = ?1 AND seen_at < ?2",
            params![drive_id, since],
        )?;
        Ok(n as i64)
    }

    // -- tags ---------------------------------------------------------------

    /// Resolve a hash (full or prefix) to a single indexed `(algo, hash)`.
    /// Errors if nothing matches or the prefix is ambiguous.
    pub fn resolve_hash(&self, prefix: &str) -> Result<(String, String)> {
        let mut stmt = self
            .conn
            .prepare("SELECT algo, hash FROM content WHERE hash LIKE ?1 LIMIT 2")?;
        let rows = stmt
            .query_map(params![format!("{prefix}%")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match rows.len() {
            0 => anyhow::bail!("no indexed content matching hash {prefix}"),
            1 => Ok(rows.into_iter().next().unwrap()),
            _ => anyhow::bail!("hash prefix {prefix} is ambiguous"),
        }
    }

    /// Attach a tag to a content hash. Returns false if it was already present.
    pub fn add_tag(&self, algo: &str, hash: &str, tag: &str) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO tags (algo, hash, tag, added_at) VALUES (?1, ?2, ?3, ?4)",
            params![algo, hash, tag, now_rfc3339()],
        )?;
        Ok(n > 0)
    }

    /// Remove a tag from a content hash. Returns false if it wasn't present.
    pub fn remove_tag(&self, algo: &str, hash: &str, tag: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM tags WHERE algo = ?1 AND hash = ?2 AND tag = ?3",
            params![algo, hash, tag],
        )?;
        Ok(n > 0)
    }

    /// All tags on a content hash, sorted.
    pub fn list_tags(&self, algo: &str, hash: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag FROM tags WHERE algo = ?1 AND hash = ?2 ORDER BY tag")?;
        let rows = stmt
            .query_map(params![algo, hash], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -- links --------------------------------------------------------------

    /// Link a set of hashes into one group (e.g. a JPG + its RAW). If any are
    /// already linked, their groups are merged. Returns the resulting group id.
    pub fn link_hashes(&mut self, members: &[(String, String)]) -> Result<String> {
        let tx = self.conn.transaction()?;
        // Existing groups touched by these members.
        let mut groups: Vec<String> = Vec::new();
        for (algo, hash) in members {
            let g: Option<String> = tx
                .query_row(
                    "SELECT group_id FROM link_members WHERE algo = ?1 AND hash = ?2",
                    params![algo, hash],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(g) = g {
                if !groups.contains(&g) {
                    groups.push(g);
                }
            }
        }
        let target = groups
            .first()
            .cloned()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Merge any other existing groups into the target.
        for g in groups.iter().skip(1) {
            tx.execute(
                "UPDATE link_members SET group_id = ?1 WHERE group_id = ?2",
                params![target, g],
            )?;
        }
        let now = now_rfc3339();
        for (algo, hash) in members {
            tx.execute(
                "INSERT OR IGNORE INTO link_members (group_id, algo, hash, added_at) VALUES (?1,?2,?3,?4)",
                params![target, algo, hash, now],
            )?;
        }
        tx.commit()?;
        Ok(target)
    }

    /// Remove a hash from its link group. If only one member would remain, the
    /// group is dissolved (a link needs at least two members). Returns whether
    /// the hash was linked.
    pub fn unlink_hash(&mut self, algo: &str, hash: &str) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let group: Option<String> = tx
            .query_row(
                "SELECT group_id FROM link_members WHERE algo = ?1 AND hash = ?2",
                params![algo, hash],
                |r| r.get(0),
            )
            .optional()?;
        let Some(group) = group else {
            return Ok(false);
        };
        tx.execute(
            "DELETE FROM link_members WHERE algo = ?1 AND hash = ?2",
            params![algo, hash],
        )?;
        let remaining: i64 = tx.query_row(
            "SELECT COUNT(*) FROM link_members WHERE group_id = ?1",
            params![group],
            |r| r.get(0),
        )?;
        if remaining < 2 {
            tx.execute(
                "DELETE FROM link_members WHERE group_id = ?1",
                params![group],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The members of the link group containing `(algo, hash)`, sorted by hash
    /// (empty if the hash is not linked).
    pub fn list_group(&self, algo: &str, hash: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT m2.algo, m2.hash
            FROM link_members m1
            JOIN link_members m2 ON m1.group_id = m2.group_id
            WHERE m1.algo = ?1 AND m1.hash = ?2
            ORDER BY m2.hash
            "#,
        )?;
        let rows = stmt
            .query_map(params![algo, hash], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Resolve a hash (full or prefix) to a source for thumbnailing:
    /// `(algo, hash, has_thumb, absolute source path)`. Prefers online drives.
    pub fn thumb_lookup(
        &self,
        hash_prefix: &str,
    ) -> Result<Option<(String, String, bool, PathBuf)>> {
        let row = self
            .conn
            .query_row(
                r#"
                SELECT c.algo, c.hash, c.has_thumb, dr.last_root, l.path
                FROM content c
                JOIN locations l ON l.algo = c.algo AND l.hash = c.hash
                JOIN drives dr   ON dr.drive_id = l.drive_id AND dr.detached = 0
                WHERE c.hash LIKE ?1
                ORDER BY dr.online DESC
                LIMIT 1
                "#,
                params![format!("{hash_prefix}%")],
                |r| {
                    let algo: String = r.get(0)?;
                    let hash: String = r.get(1)?;
                    let has_thumb: bool = r.get::<_, i64>(2)? != 0;
                    let root: String = r.get(3)?;
                    let path: String = r.get(4)?;
                    Ok((algo, hash, has_thumb, PathBuf::from(root).join(path)))
                },
            )
            .optional()?;
        Ok(row)
    }

    // -- logical filesystem -------------------------------------------------

    /// List the immediate children of `path` on `drive` (use "" for the drive
    /// root). Subdirectories are synthesized from file paths; files carry their
    /// content summary. Paths use forward slashes, matching `locations.path`.
    #[cfg(feature = "serve")]
    pub fn list_dir(&self, drive: &str, path: &str) -> Result<Vec<DirEntry>> {
        let path = path.trim_matches('/');
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        // `plen` is a CHAR count (SQLite substr/instr are char-based).
        let plen = prefix.chars().count() as i64;
        // Range bound to scan only this subtree (avoids LIKE metachar issues).
        let (lo, hi) = if prefix.is_empty() {
            (String::new(), String::new())
        } else {
            (prefix.clone(), prefix_upper(&prefix))
        };
        let range = if prefix.is_empty() {
            ""
        } else {
            "AND l.path >= ?2 AND l.path < ?3"
        };

        let mut out: Vec<DirEntry> = Vec::new();

        // Files directly in `path` (no further '/').
        let files_sql = format!(
            r#"
            SELECT substr(l.path, ?4 + 1) AS name, l.algo, l.hash,
                   c.size, c.file_type, c.ext, c.has_thumb
            FROM locations l
            JOIN content c ON c.algo = l.algo AND c.hash = l.hash
            WHERE l.drive_id = ?1 {range}
              AND instr(substr(l.path, ?4 + 1), '/') = 0
            ORDER BY name
            "#
        );
        {
            let mut stmt = self.conn.prepare(&files_sql)?;
            let rows = stmt.query_map(params![drive, lo, hi, plen], |r| {
                Ok(DirEntry {
                    name: r.get(0)?,
                    kind: "file".to_string(),
                    algo: r.get(1)?,
                    hash: r.get(2)?,
                    size: r.get(3)?,
                    file_type: r.get(4)?,
                    ext: r.get(5)?,
                    has_thumb: Some(r.get::<_, i64>(6)? != 0),
                    file_count: None,
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }

        // Subdirectories: the next path segment among deeper entries.
        let dirs_sql = format!(
            r#"
            SELECT child, COUNT(*) AS n FROM (
                SELECT substr(rest, 1, instr(rest, '/') - 1) AS child
                FROM (SELECT substr(l.path, ?4 + 1) AS rest
                      FROM locations l WHERE l.drive_id = ?1 {range})
                WHERE instr(rest, '/') > 0
            )
            GROUP BY child ORDER BY child
            "#
        );
        {
            let mut stmt = self.conn.prepare(&dirs_sql)?;
            let rows = stmt.query_map(params![drive, lo, hi, plen], |r| {
                Ok(DirEntry {
                    name: r.get(0)?,
                    kind: "dir".to_string(),
                    algo: None,
                    hash: None,
                    size: None,
                    file_type: None,
                    ext: None,
                    has_thumb: None,
                    file_count: Some(r.get(1)?),
                })
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// Describe a single logical path: a file (with its content summary) or a
    /// directory (with a descendant file count). `None` if nothing matches.
    #[cfg(feature = "serve")]
    pub fn stat(&self, drive: &str, path: &str) -> Result<Option<DirEntry>> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            // The drive root is a directory.
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM locations WHERE drive_id = ?1",
                params![drive],
                |r| r.get(0),
            )?;
            return Ok(Some(DirEntry::dir(String::new(), n)));
        }
        // File?
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let file = self
            .conn
            .query_row(
                r#"
                SELECT l.algo, l.hash, c.size, c.file_type, c.ext, c.has_thumb
                FROM locations l JOIN content c ON c.algo=l.algo AND c.hash=l.hash
                WHERE l.drive_id = ?1 AND l.path = ?2
                "#,
                params![drive, path],
                |r| {
                    Ok(DirEntry {
                        name: name.clone(),
                        kind: "file".to_string(),
                        algo: r.get(0)?,
                        hash: r.get(1)?,
                        size: r.get(2)?,
                        file_type: r.get(3)?,
                        ext: r.get(4)?,
                        has_thumb: Some(r.get::<_, i64>(5)? != 0),
                        file_count: None,
                    })
                },
            )
            .optional()?;
        if let Some(f) = file {
            return Ok(Some(f));
        }
        // Directory? (any descendant)
        let prefix = format!("{path}/");
        let hi = prefix_upper(&prefix);
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM locations WHERE drive_id = ?1 AND path >= ?2 AND path < ?3",
            params![drive, prefix, hi],
            |r| r.get(0),
        )?;
        if n > 0 {
            Ok(Some(DirEntry::dir(name, n)))
        } else {
            Ok(None)
        }
    }

    /// Resolve a hash (full or prefix) to an absolute readable path, preferring
    /// online drives. Returns `(algo, hash, abs path)`.
    #[cfg(feature = "serve")]
    pub fn content_source(&self, hash_prefix: &str) -> Result<Option<(String, String, PathBuf)>> {
        let row = self
            .conn
            .query_row(
                r#"
                SELECT c.algo, c.hash, dr.last_root, l.path
                FROM content c
                JOIN locations l ON l.algo = c.algo AND l.hash = c.hash
                JOIN drives dr   ON dr.drive_id = l.drive_id AND dr.detached = 0
                WHERE c.hash LIKE ?1
                ORDER BY dr.online DESC
                LIMIT 1
                "#,
                params![format!("{hash_prefix}%")],
                |r| {
                    let algo: String = r.get(0)?;
                    let hash: String = r.get(1)?;
                    let root: String = r.get(2)?;
                    let path: String = r.get(3)?;
                    Ok((algo, hash, PathBuf::from(root).join(path)))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Full detail for a content hash: summary, metadata, locations, tags, links.
    #[cfg(feature = "serve")]
    pub fn content_detail(&self, hash_prefix: &str) -> Result<Option<ContentDetail>> {
        let (algo, hash) = match self.resolve_hash(hash_prefix) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        let summary = self.conn.query_row(
            "SELECT size, file_type, ext, extracted_at, has_thumb FROM content WHERE algo=?1 AND hash=?2",
            params![algo, hash],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)? != 0,
                ))
            },
        )?;
        let metadata = {
            let mut stmt = self.conn.prepare(
                "SELECT grp, key, value FROM metadata WHERE algo=?1 AND hash=?2 ORDER BY grp, key",
            )?;
            let rows = stmt.query_map(params![algo, hash], |r| {
                Ok(MetaTag {
                    group: r.get(0)?,
                    key: r.get(1)?,
                    value: r.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let locations = {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT l.drive_id, l.path, dr.online
                FROM locations l JOIN drives dr ON dr.drive_id = l.drive_id
                WHERE l.algo=?1 AND l.hash=?2 ORDER BY l.drive_id, l.path
                "#,
            )?;
            let rows = stmt.query_map(params![algo, hash], |r| {
                Ok(LocationRow {
                    drive_id: r.get(0)?,
                    path: r.get(1)?,
                    online: r.get::<_, i64>(2)? != 0,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let tags = self.list_tags(&algo, &hash)?;
        let links = self
            .list_group(&algo, &hash)?
            .into_iter()
            .map(|(_, h)| h)
            .filter(|h| *h != hash)
            .collect();
        Ok(Some(ContentDetail {
            algo,
            hash,
            size: summary.0,
            file_type: summary.1,
            ext: summary.2,
            extracted_at: summary.3,
            has_thumb: summary.4,
            metadata,
            locations,
            tags,
            links,
        }))
    }

    // -- query --------------------------------------------------------------

    /// Paginated query grouped by content hash. Built dynamically from the
    /// active filters; never materializes the whole index.
    pub fn query(&self, f: &QueryFilter) -> Result<Vec<QueryRow>> {
        let mut where_sql: Vec<String> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ft) = &f.file_type {
            where_sql.push("c.file_type = ?".into());
            args.push(Box::new(ft.clone()));
        }
        if let Some(ext) = &f.ext {
            where_sql.push("c.ext = ?".into());
            args.push(Box::new(ext.clone()));
        }
        if let Some(hp) = &f.hash_prefix {
            where_sql.push("c.hash LIKE ?".into());
            args.push(Box::new(format!("{hp}%")));
        }
        if let Some(k) = &f.key {
            // Restrict to hashes carrying this metadata key (and value if given).
            if let Some(v) = &f.value {
                where_sql.push(
                    "EXISTS (SELECT 1 FROM metadata m WHERE m.algo=c.algo AND m.hash=c.hash AND m.key=? AND m.value=?)"
                        .into(),
                );
                args.push(Box::new(k.clone()));
                args.push(Box::new(v.clone()));
            } else {
                where_sql.push(
                    "EXISTS (SELECT 1 FROM metadata m WHERE m.algo=c.algo AND m.hash=c.hash AND m.key=?)"
                        .into(),
                );
                args.push(Box::new(k.clone()));
            }
        }
        if let Some(d) = &f.drive_id {
            where_sql.push(
                "EXISTS (SELECT 1 FROM locations l WHERE l.algo=c.algo AND l.hash=c.hash AND l.drive_id=?)"
                    .into(),
            );
            args.push(Box::new(d.clone()));
        }
        // A specific tag, or the favorite tag via the `favorite` flag.
        let tag_filter = f
            .tag
            .clone()
            .or_else(|| f.favorite.then(|| FAVORITE_TAG.to_string()));
        if let Some(t) = tag_filter {
            where_sql.push(
                "EXISTS (SELECT 1 FROM tags t WHERE t.algo=c.algo AND t.hash=c.hash AND t.tag=?)"
                    .into(),
            );
            args.push(Box::new(t));
        }
        if f.offline_only {
            // No location on any currently-online, non-detached drive.
            where_sql.push(
                "NOT EXISTS (SELECT 1 FROM locations l JOIN drives dr ON dr.drive_id=l.drive_id \
                 WHERE l.algo=c.algo AND l.hash=c.hash AND dr.online=1 AND dr.detached=0)"
                    .into(),
            );
        }

        let where_clause = if where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_sql.join(" AND "))
        };

        let sql = format!(
            r#"
            SELECT c.algo, c.hash, c.file_type, c.ext, c.size, c.has_thumb,
                   (SELECT COUNT(*) FROM locations l WHERE l.algo=c.algo AND l.hash=c.hash),
                   (SELECT MIN(l.path) FROM locations l WHERE l.algo=c.algo AND l.hash=c.hash),
                   (SELECT IFNULL(GROUP_CONCAT(DISTINCT l.drive_id),'') FROM locations l WHERE l.algo=c.algo AND l.hash=c.hash),
                   EXISTS (SELECT 1 FROM locations l JOIN drives dr ON dr.drive_id=l.drive_id
                           WHERE l.algo=c.algo AND l.hash=c.hash AND dr.online=1 AND dr.detached=0),
                   (SELECT IFNULL(GROUP_CONCAT(t.tag),'') FROM tags t WHERE t.algo=c.algo AND t.hash=c.hash),
                   (SELECT lm.group_id FROM link_members lm WHERE lm.algo=c.algo AND lm.hash=c.hash)
            FROM content c
            {where_clause}
            ORDER BY c.hash
            LIMIT ? OFFSET ?
            "#
        );
        args.push(Box::new(f.limit));
        args.push(Box::new(f.offset));

        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(args.iter().map(|b| b.as_ref()));
        let rows = stmt
            .query_map(params, |r| {
                Ok(QueryRow {
                    algo: r.get(0)?,
                    hash: r.get(1)?,
                    file_type: r.get(2)?,
                    ext: r.get(3)?,
                    size: r.get(4)?,
                    has_thumb: r.get::<_, i64>(5)? != 0,
                    locations: r.get(6)?,
                    sample_path: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    drives: r.get(8)?,
                    online: r.get::<_, i64>(9)? != 0,
                    tags: r.get(10)?,
                    link_group: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
