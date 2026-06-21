//! Integration test for the index store: build a tiny manifest + meta file on
//! disk, index them, and exercise every query filter.

use std::collections::BTreeMap;
use std::path::PathBuf;

use hashit_core::manifest::{FileEntry, Manifest};
use hashit_core::meta::MetaFile;

// The store module is private to the binary crate, so re-declare the path to it
// is not possible; instead we drive it through a small include. We test it via
// the crate's library surface by importing the module file directly.
#[path = "../src/store.rs"]
#[allow(dead_code)] // FileRow fields are read by the binary, not every test.
mod store;

// `store` references `crate::query`, so pull the query module in too.
#[path = "../src/query/mod.rs"]
mod query;

use store::{QueryParams, Store};

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("hashit-idx-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn entry(size: u64, hash: &str) -> FileEntry {
    FileEntry {
        size,
        mtime_ns: 1_700_000_000_000_000_000,
        hash: hash.to_string(),
        algo: "blake3".to_string(),
        flags: vec![],
        hashed_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

#[test]
fn index_and_query() {
    let root = unique_dir("root");
    let meta_folder = unique_dir("meta");
    let db = unique_dir("db").join("index.db");

    // A directory manifest with two files (one image, one text).
    let mut files = BTreeMap::new();
    files.insert("photo.jpg".to_string(), entry(2000, "abcd1234ef567890"));
    files.insert("notes.txt".to_string(), entry(10, "deadbeef00112233"));
    Manifest::new(files).save(&root).unwrap();

    // Metadata for the image hash only: one EXIF tag + one user property.
    let mut m = MetaFile {
        hash: "abcd1234ef567890".to_string(),
        algo: "blake3".to_string(),
        size: 2000,
        file_type: Some("image".to_string()),
        ext: Some("jpg".to_string()),
        extractor_version: 1,
        tags: BTreeMap::from([("EXIF:Model".to_string(), "Canon".to_string())]),
        properties: BTreeMap::from([("rating".to_string(), "5".to_string())]),
        updated_at: String::new(),
    };
    m.save(&meta_folder).unwrap();

    let mut s = Store::open(&db).unwrap();
    s.rebuild(std::slice::from_ref(&root), &meta_folder).unwrap();

    // Stats: 2 files, 2 hashes, 2 meta rows (1 tag + 1 property).
    assert_eq!(s.stats().unwrap(), (2, 2, 2));

    // Name substring.
    let (rows, total) = s
        .query(&QueryParams {
            name: "photo".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(rows[0].name, "photo.jpg");
    assert_eq!(rows[0].file_type.as_deref(), Some("image"));

    // Hash prefix.
    let (rows, _) = s
        .query(&QueryParams {
            hash: "abcd".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].hash, "abcd1234ef567890");

    // Size range excludes the tiny text file.
    let (_, total) = s
        .query(&QueryParams {
            min_size: Some(1000),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total, 1);

    // EXIF tag filter (key + value).
    let (rows, _) = s
        .query(&QueryParams {
            tags: vec![("EXIF:Model".to_string(), Some("Canon".to_string()))],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);

    // User property filter, value-less (key present).
    let (rows, _) = s
        .query(&QueryParams {
            tags: vec![("rating".to_string(), None)],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);

    // Wrong tag value matches nothing.
    let (_, total) = s
        .query(&QueryParams {
            tags: vec![("EXIF:Model".to_string(), Some("Nikon".to_string()))],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(total, 0);

    // Incremental: remove the manifest, re-sync the dir → its files drop out.
    Manifest::delete(&root).unwrap();
    s.sync_dir(&root).unwrap();
    assert_eq!(s.stats().unwrap().0, 0);

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&meta_folder).ok();
    std::fs::remove_dir_all(db.parent().unwrap()).ok();
}

#[test]
fn corrupt_manifest_is_skipped() {
    let root = unique_dir("bad-root");
    let meta_folder = unique_dir("bad-meta");
    let db = unique_dir("bad-db").join("index.db");

    // A good manifest in the root and a corrupt `.hashit` in a subdirectory.
    let mut files = BTreeMap::new();
    files.insert("notes.txt".to_string(), entry(10, "deadbeef00112233"));
    Manifest::new(files).save(&root).unwrap();

    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".hashit"), b"{ not valid json").unwrap();

    let mut s = Store::open(&db).unwrap();
    // The corrupt manifest must not abort the whole build; the good one indexes.
    s.rebuild(std::slice::from_ref(&root), &meta_folder).unwrap();
    assert_eq!(s.stats().unwrap().0, 1);

    // Incrementally re-syncing the corrupt dir is also a no-op, not an error.
    s.sync_dir(&sub).unwrap();
    assert_eq!(s.stats().unwrap().0, 1);

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&meta_folder).ok();
    std::fs::remove_dir_all(db.parent().unwrap()).ok();
}

#[test]
fn ignored_directories_are_not_indexed() {
    let root = unique_dir("ign-root");
    let meta_folder = unique_dir("ign-meta");
    let db = unique_dir("ign-db").join("index.db");

    // One file in the root, one in a `cache` subdirectory we'll ignore.
    let mut top = BTreeMap::new();
    top.insert("notes.txt".to_string(), entry(10, "deadbeef00112233"));
    Manifest::new(top).save(&root).unwrap();

    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let mut nested = BTreeMap::new();
    nested.insert("blob.bin".to_string(), entry(2000, "abcd1234ef567890"));
    Manifest::new(nested).save(&cache).unwrap();

    let mut s = Store::open(&db).unwrap();
    s.set_ignores(vec!["cache".to_string()]);
    s.rebuild(std::slice::from_ref(&root), &meta_folder).unwrap();

    // Only the root file is indexed; the ignored subtree is skipped.
    assert_eq!(s.stats().unwrap().0, 1);
    let (rows, _) = s
        .query(&QueryParams {
            name: "blob".to_string(),
            ..Default::default()
        })
        .unwrap();
    assert!(rows.is_empty());

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&meta_folder).ok();
    std::fs::remove_dir_all(db.parent().unwrap()).ok();
}

#[test]
fn query_language_end_to_end() {
    let root = unique_dir("ql-root");
    let meta_folder = unique_dir("ql-meta");
    let db = unique_dir("ql-db").join("index.db");

    let mut files = BTreeMap::new();
    files.insert("photo.jpg".to_string(), entry(2000, "abcd1234ef567890"));
    files.insert("notes.txt".to_string(), entry(10, "deadbeef00112233"));
    Manifest::new(files).save(&root).unwrap();

    let mut m = MetaFile {
        hash: "abcd1234ef567890".to_string(),
        algo: "blake3".to_string(),
        size: 2000,
        file_type: Some("image".to_string()),
        ext: Some("jpg".to_string()),
        extractor_version: 1,
        tags: BTreeMap::from([("EXIF:Model".to_string(), "Canon".to_string())]),
        properties: BTreeMap::from([("rating".to_string(), "5".to_string())]),
        updated_at: String::new(),
    };
    m.save(&meta_folder).unwrap();

    let mut s = Store::open(&db).unwrap();
    s.rebuild(std::slice::from_ref(&root), &meta_folder).unwrap();

    // The entries' mtime (1.7e18 ns) is 2023-11-14, shared by both files.
    let run = |q: &str| {
        let ast = query::parse(q).unwrap();
        let filter = query::lower(&ast, chrono::Local::now()).unwrap();
        s.query_filter(&filter).unwrap().1
    };

    assert_eq!(run("photo"), 1); // default field set (name/path/hash)
    assert_eq!(run("name:notes"), 1);
    assert_eq!(run("ext:jpg"), 1);
    assert_eq!(run("size:1000.."), 1); // open range, only the 2000-byte file
    assert_eq!(run("-type:image"), 1); // negation → the text file (no content row)
    assert_eq!(run("rating:5"), 1); // user-property value, substring
    assert_eq!(run(r#""EXIF:Model":Canon"#), 1); // quoted field name with colons
    assert_eq!(run("exif-model:Canon"), 1); // '-' alias for ':', case-insensitive key
    assert_eq!(run("exif-model:Nikon"), 0); // right key, wrong value
    assert_eq!(run("mtime:{2023}"), 2); // both share a 2023 mtime
    assert_eq!(run("mtime:{2024}"), 0);
    assert_eq!(run("mtime:..{2020}"), 0); // open low bound
    assert_eq!(run("photo +ext:jpg"), 1); // AND of two terms
    assert_eq!(run("photo -ext:jpg"), 0); // contradictory terms

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&meta_folder).ok();
    std::fs::remove_dir_all(db.parent().unwrap()).ok();
}

#[test]
fn dir_and_operator_queries() {
    let root = unique_dir("op-root");
    let meta_folder = unique_dir("op-meta");
    let db = unique_dir("op-db").join("index.db");

    // One file directly in `root`, one in a `root/sub` subdirectory; each
    // directory gets its own `.hashit` manifest.
    let mut top = BTreeMap::new();
    top.insert("notes.txt".to_string(), entry(10, "deadbeef00112233"));
    Manifest::new(top).save(&root).unwrap();

    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let mut nested = BTreeMap::new();
    nested.insert("photo.jpg".to_string(), entry(2000, "abcd1234ef567890"));
    Manifest::new(nested).save(&sub).unwrap();

    let mut s = Store::open(&db).unwrap();
    s.rebuild(std::slice::from_ref(&root), &meta_folder).unwrap();

    let run = |q: &str| {
        let ast = query::parse(q).unwrap();
        let filter = query::lower(&ast, chrono::Local::now()).unwrap();
        s.query_filter(&filter).unwrap().1
    };

    let root_s = root.to_string_lossy();
    let sub_s = sub.to_string_lossy();

    // `=` exact: only files directly in that directory.
    assert_eq!(run(&format!("dir={root_s}")), 1); // just notes.txt
    assert_eq!(run(&format!("dir={sub_s}")), 1); // just photo.jpg

    // `:=` LIKE glob: the whole subtree under root (both files).
    assert_eq!(run(&format!("dir:={root_s}/%")), 1); // descendants only → photo.jpg
    assert_eq!(run(&format!("dir:={root_s}%")), 2); // root and below → both

    // `:` substring still works on dir.
    assert_eq!(run("dir:sub"), 1);

    // Exact vs substring on a text field.
    assert_eq!(run("name=notes.txt"), 1);
    assert_eq!(run("name=notes"), 0); // exact: no partial credit
    assert_eq!(run("name:notes"), 1); // substring

    // `=`/`:=` are text-only for now.
    assert!(query::lower(&query::parse("size=10").unwrap(), chrono::Local::now()).is_err());

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&meta_folder).ok();
    std::fs::remove_dir_all(db.parent().unwrap()).ok();
}
