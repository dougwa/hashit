# Architecture

`hashit` is a cargo workspace of two binaries over a shared core. The design
goal is a clean separation between **writing** (the `hashit` scanner, which only
ever touches individual files under its roots) and the **global read view** (the
`hashit-idx` daemon, which indexes and searches but never writes).

## Crates

| Crate | Responsibility | Notable deps |
|-------|----------------|--------------|
| `hashit-core` | The portable data model and engines: `.hashit` manifests, hashing, the scan engine, hash-aware file ops, per-hash metadata files, and a manifest walker. Dependency-light so both binaries build fast. | serde, blake3, sha2, walkdir, rayon, glob, chrono |
| `hashit-extract` | Extraction engine: file-type detection + EXIF (`exiftool_rs`), thumbnails and previews (`magick-*`). Free functions, swappable for an external-tool fallback later. | exiftool_rs, magick-core, magick-codecs |
| `hashit` | The scanner CLI binary, plus the `watch --serve` gRPC FileOps server. | hashit-core, hashit-extract, clap, notify, tonic, tokio |
| `hashit-idx` | The index + gRPC Search daemon binary. | hashit-core, rusqlite, notify, tonic, tokio |

The workspace split keeps `hashit-idx` from ever compiling the image/EXIF crates
and keeps `hashit` from compiling SQLite.

## `hashit-core`

- **`manifest.rs`** — `Manifest { version, updated_at, files: BTreeMap<String, FileEntry> }`,
  one `.hashit` per directory. `FileEntry { size, mtime_ns, hash, algo, flags, hashed_at }`
  derives `Eq` so a rebuilt manifest can be compared to the old one and **skip
  writes on no-op scans**. Atomic save (temp file + fsync + rename). `ManifestCache`
  lazily loads a directory's manifest so `cp`/`mv` (and `get`/`put-meta`) can look
  up a file's entry without re-hashing.
- **`hash.rs`** — `HashAlgo` (Blake3/Sha256) and streaming `hash_file`.
- **`scan.rs`** — `process_dir(dir, opts, root)` is the single source of truth for
  keeping one directory's manifest correct (list files, apply the recompute
  heuristic, hash pending files in parallel, drop missing entries, write only if
  changed). `scan(root, opts)` runs `process_dir` across every directory in
  parallel. Also home to the `is_apple_double` / `is_dedup_pointer` predicates.
- **`fileops.rs`** — hash-aware `cp`/`mv`/`rm`: scan sources, carry source entries
  into target manifests (no re-hash), reconcile affected directories.
- **`meta.rs`** — the per-hash metadata file format and sharded path math
  (`<meta-folder>/<h0:2>/<h2:4>/<hash>.{meta.json,thumbnail.jpg,preview.jpg}`).
  `MetaFile { hash, algo, size, file_type, ext, extractor_version, tags, properties, updated_at }`
  with atomic save. Artifact presence is a plain file-existence check, not stored.
- **`walk.rs`** — `for_each_entry(root, f)` enumerates every file recorded in every
  `.hashit` under a root. Shared by the metadata pass and the index build.

## `hashit` (scanner CLI)

Commands: `scan`, `watch`, `cp`, `mv`, `rm`, `get-meta`, `put-meta`.

- **Metadata pass (`meta_pass.rs`)** — after a scan (or per changed dir under
  `watch`), walks the manifests, dedups by hash, and for each hash **missing** a
  requested artifact reads one representative copy and calls `hashit-extract` to
  produce it (parallel via rayon). Idempotent: existing artifacts are left alone,
  and existing user `properties` are preserved.
- **get/put-meta (`metacmd.rs`)** — resolve a path → its manifest entry → content
  hash, then read or edit `<hash>.meta.json`. Edits are hash-keyed, so they apply
  to every copy. This module is shared by the CLI and the gRPC server.
- **Watcher (`watch.rs`)** — initial `scan`, then a `notify` debounced watcher;
  each event batch maps to the set of affected directories, re-run through
  `process_dir`. An `on_dir` callback lets `watch` drive the metadata pass per
  changed directory. Own writes (`.hashit`, `._*`) are filtered to avoid loops.
- **FileOps server (`serve.rs`)** — `watch --serve` starts a tonic gRPC server
  (`hashit.fileops.v1.FileOps`) bound to localhost, exposing
  `cp`/`mv`/`rm`/`get-meta`/`put-meta`. Each RPC routes through the same
  `core::fileops` + `metacmd` code as the CLI. The watcher runs on a background
  thread while the server runs on a tokio runtime.

## `hashit-idx` (index + search daemon)

- **Store (`store.rs`)** — a rebuildable SQLite index (WAL, `schema_meta` version
  row). Tables: `files(path PK, dir, name, algo, hash, size, mtime_ns)`,
  `content(algo, hash → file_type, ext)`, and `meta_kv(algo, hash, kind, key, value)`
  (`kind` = tag | property), indexed for reverse lookups. `rebuild` walks each
  root's `.hashit` (via `core::walk`) into `files` and every `*.meta.json` into
  `content`/`meta_kv`. `sync_dir`/`sync_meta` apply incremental updates. `query`
  builds dynamic SQL: substring on name, prefix on hash, range scans on
  size/mtime, and an `EXISTS` join on `meta_kv` per tag filter.
- **Watcher (`watcher.rs`)** — a `notify` debounced watcher over the roots (for
  `.hashit`) and the meta folder (for `*.meta.json`). A `.hashit` change re-syncs
  that directory's files; a `<hash>.meta.json` change re-syncs that hash's
  content/tags. Thumbnail/preview JPEGs are ignored — they aren't searchable.
- **Search service (`search.rs`)** — a thin tonic mapping from
  `hashit.search.v1.Search` (`Query`, `Stats`) to `store` calls. The store sits
  behind an `Arc<Mutex<…>>` shared with the watcher thread; queries are synchronous
  and never hold the lock across an `.await`.

## gRPC contracts

Defined under `proto/` and compiled by each binary's `build.rs` using a vendored
`protoc` (`protoc-bin-vendored`), so no system protobuf compiler is required:

- `proto/fileops.proto` → `hashit.fileops.v1.FileOps` (server in `hashit`).
- `proto/search.proto` → `hashit.search.v1.Search` (server in `hashit-idx`).

Both services bind to `127.0.0.1` only.

## Key invariants

- A `.hashit` describes only the files directly in its directory; recursion is
  implicit via one manifest per directory, so moving/removing a whole directory
  needs no parent-manifest update.
- Carried entries (cp/mv) re-read on-disk size/mtime so a later scan sees the file
  as `unchanged` and doesn't re-hash — holds even on exFAT's coarse mtime.
- Metadata and the index are keyed by `(algo, hash)`, so different algorithms
  never collide and identical content shares one metadata record.
- The metadata folder and the index are derived data: delete and regenerate at
  will. Only user-authored `properties` in `meta.json` are irrecoverable state.
