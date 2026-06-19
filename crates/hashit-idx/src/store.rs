//! The read-model: a rebuildable SQLite index over `.hashit` manifests and
//! `.hashit-meta/*.meta.json` files.
//!
//! Everything here is derived from those files, so the database can be deleted
//! and rebuilt at any time. `hashit-idx` only ever reads the source files;
//! mutations happen through `hashit`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection, ToSql};

use hashit_core::manifest::Manifest;
use hashit_core::meta::{MetaFile, META_SUFFIX};
use hashit_core::walk;

const SCHEMA_VERSION: i64 = 1;

/// Filters for a search, decoupled from the gRPC types.
#[derive(Default)]
pub struct QueryParams {
    pub name: String,
    pub hash: String,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub min_mtime_ns: Option<u64>,
    pub max_mtime_ns: Option<u64>,
    /// (key, optional value) — value `None` matches any value for the key.
    pub tags: Vec<(String, Option<String>)>,
    pub limit: u32,
    pub offset: u32,
}

/// One matched file.
pub struct FileRow {
    pub path: String,
    pub name: String,
    pub hash: String,
    pub algo: String,
    pub size: u64,
    pub mtime_ns: u64,
    pub file_type: Option<String>,
    pub ext: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the index database, in WAL mode.
    pub fn open(db_path: &Path) -> Result<Store> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let store = Store { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_meta (version INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS files (
                path     TEXT PRIMARY KEY,
                dir      TEXT NOT NULL,
                name     TEXT NOT NULL,
                algo     TEXT NOT NULL,
                hash     TEXT NOT NULL,
                size     INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS files_dir   ON files(dir);
            CREATE INDEX IF NOT EXISTS files_hash  ON files(hash);
            CREATE INDEX IF NOT EXISTS files_name  ON files(name);
            CREATE INDEX IF NOT EXISTS files_size  ON files(size);
            CREATE INDEX IF NOT EXISTS files_mtime ON files(mtime_ns);

            CREATE TABLE IF NOT EXISTS content (
                algo      TEXT NOT NULL,
                hash      TEXT NOT NULL,
                file_type TEXT,
                ext       TEXT,
                PRIMARY KEY (algo, hash)
            );

            CREATE TABLE IF NOT EXISTS meta_kv (
                algo  TEXT NOT NULL,
                hash  TEXT NOT NULL,
                kind  TEXT NOT NULL,
                key   TEXT NOT NULL,
                value TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS meta_kv_hash ON meta_kv(hash);
            CREATE INDEX IF NOT EXISTS meta_kv_kv   ON meta_kv(key, value);
            "#,
        )?;
        let has_version: bool =
            self.conn
                .query_row("SELECT COUNT(*) FROM schema_meta", [], |r| {
                    Ok(r.get::<_, i64>(0)? > 0)
                })?;
        if !has_version {
            self.conn
                .execute("INSERT INTO schema_meta (version) VALUES (?1)", params![SCHEMA_VERSION])?;
        }
        Ok(())
    }

    /// Drop all rows and rebuild from scratch by walking the roots and meta folder.
    pub fn rebuild(&mut self, roots: &[PathBuf], meta_folder: &Path) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM files; DELETE FROM content; DELETE FROM meta_kv;")?;
        for root in roots {
            self.reindex_root(root)?;
        }
        self.reindex_meta_folder(meta_folder)?;
        Ok(())
    }

    /// Insert every file recorded in every `.hashit` under `root`.
    fn reindex_root(&mut self, root: &Path) -> Result<()> {
        let mut rows: Vec<(String, String, String, String, String, i64, i64)> = Vec::new();
        walk::for_each_entry(root, |e| {
            let dir = e
                .path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            rows.push((
                e.path.to_string_lossy().to_string(),
                dir,
                e.name,
                e.entry.algo,
                e.entry.hash,
                e.entry.size as i64,
                e.entry.mtime_ns as i64,
            ));
        })?;
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO files (path, dir, name, algo, hash, size, mtime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for r in &rows {
                stmt.execute(params![r.0, r.1, r.2, r.3, r.4, r.5, r.6])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Read every `*.meta.json` under the meta folder and index its content.
    fn reindex_meta_folder(&mut self, meta_folder: &Path) -> Result<()> {
        if !meta_folder.exists() {
            return Ok(());
        }
        let mut hashes: Vec<String> = Vec::new();
        for dirent in walkdir::WalkDir::new(meta_folder)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !dirent.file_type().is_file() {
                continue;
            }
            if let Some(hash) = hash_from_meta_filename(dirent.file_name().to_string_lossy().as_ref())
            {
                hashes.push(hash.to_string());
            }
        }
        for hash in hashes {
            self.sync_meta(meta_folder, &hash)?;
        }
        Ok(())
    }

    /// Re-sync the `files` rows for one directory from its current `.hashit`
    /// (handles create/modify/remove of the manifest).
    pub fn sync_dir(&mut self, dir: &Path) -> Result<()> {
        let dir_str = dir.to_string_lossy().to_string();
        let manifest = Manifest::load(dir)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM files WHERE dir = ?1", params![dir_str])?;
        if let Some(m) = manifest {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO files (path, dir, name, algo, hash, size, mtime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (name, e) in &m.files {
                let path = dir.join(name).to_string_lossy().to_string();
                stmt.execute(params![
                    path,
                    dir_str,
                    name,
                    e.algo,
                    e.hash,
                    e.size as i64,
                    e.mtime_ns as i64
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Re-sync the content/tags for one hash from its `meta.json` (handles
    /// create/modify/remove).
    pub fn sync_meta(&mut self, meta_folder: &Path, hash: &str) -> Result<()> {
        let meta = MetaFile::load(meta_folder, hash)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM content WHERE hash = ?1", params![hash])?;
        tx.execute("DELETE FROM meta_kv WHERE hash = ?1", params![hash])?;
        if let Some(m) = meta {
            tx.execute(
                "INSERT OR REPLACE INTO content (algo, hash, file_type, ext)
                 VALUES (?1, ?2, ?3, ?4)",
                params![m.algo, m.hash, m.file_type, m.ext],
            )?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO meta_kv (algo, hash, kind, key, value) VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for (k, v) in &m.tags {
                    stmt.execute(params![m.algo, m.hash, "tag", k, v])?;
                }
                for (k, v) in &m.properties {
                    stmt.execute(params![m.algo, m.hash, "property", k, v])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Run a search; returns the matched rows and the total match count.
    pub fn query(&self, p: &QueryParams) -> Result<(Vec<FileRow>, u64)> {
        let mut where_sql = String::from(" WHERE 1=1");
        let mut binds: Vec<Box<dyn ToSql>> = Vec::new();
        if !p.name.is_empty() {
            where_sql.push_str(" AND instr(lower(f.name), lower(?))>0");
            binds.push(Box::new(p.name.clone()));
        }
        if !p.hash.is_empty() {
            where_sql.push_str(" AND f.hash LIKE ? || '%'");
            binds.push(Box::new(p.hash.clone()));
        }
        if let Some(v) = p.min_size {
            where_sql.push_str(" AND f.size >= ?");
            binds.push(Box::new(v as i64));
        }
        if let Some(v) = p.max_size {
            where_sql.push_str(" AND f.size <= ?");
            binds.push(Box::new(v as i64));
        }
        if let Some(v) = p.min_mtime_ns {
            where_sql.push_str(" AND f.mtime_ns >= ?");
            binds.push(Box::new(v as i64));
        }
        if let Some(v) = p.max_mtime_ns {
            where_sql.push_str(" AND f.mtime_ns <= ?");
            binds.push(Box::new(v as i64));
        }
        for (k, val) in &p.tags {
            match val {
                Some(v) => {
                    where_sql.push_str(
                        " AND EXISTS (SELECT 1 FROM meta_kv m WHERE m.algo=f.algo AND m.hash=f.hash AND m.key=? AND m.value=?)",
                    );
                    binds.push(Box::new(k.clone()));
                    binds.push(Box::new(v.clone()));
                }
                None => {
                    where_sql.push_str(
                        " AND EXISTS (SELECT 1 FROM meta_kv m WHERE m.algo=f.algo AND m.hash=f.hash AND m.key=?)",
                    );
                    binds.push(Box::new(k.clone()));
                }
            }
        }

        let total: u64 = {
            let sql = format!("SELECT COUNT(*) FROM files f{where_sql}");
            self.conn
                .query_row(&sql, params_from_iter(binds.iter()), |r| {
                    Ok(r.get::<_, i64>(0)? as u64)
                })?
        };

        let limit = if p.limit == 0 { 100 } else { p.limit };
        let sql = format!(
            "SELECT f.path, f.name, f.hash, f.algo, f.size, f.mtime_ns, c.file_type, c.ext
             FROM files f
             LEFT JOIN content c ON c.algo=f.algo AND c.hash=f.hash{where_sql}
             ORDER BY f.path LIMIT ? OFFSET ?"
        );
        binds.push(Box::new(limit as i64));
        binds.push(Box::new(p.offset as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(binds.iter()), |r| {
                Ok(FileRow {
                    path: r.get(0)?,
                    name: r.get(1)?,
                    hash: r.get(2)?,
                    algo: r.get(3)?,
                    size: r.get::<_, i64>(4)? as u64,
                    mtime_ns: r.get::<_, i64>(5)? as u64,
                    file_type: r.get(6)?,
                    ext: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total))
    }

    /// (files, distinct hashes, meta_kv rows).
    pub fn stats(&self) -> Result<(u64, u64, u64)> {
        let files: i64 = self.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        let hashes: i64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT hash) FROM files", [], |r| r.get(0))?;
        let tags: i64 = self.conn.query_row("SELECT COUNT(*) FROM meta_kv", [], |r| r.get(0))?;
        Ok((files as u64, hashes as u64, tags as u64))
    }
}

/// Extract the hash from a `<hash>.meta.json` filename, if it is one.
pub fn hash_from_meta_filename(name: &str) -> Option<&str> {
    let suffix = format!(".{META_SUFFIX}");
    name.strip_suffix(&suffix).filter(|h| !h.is_empty())
}
