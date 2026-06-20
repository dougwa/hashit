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
