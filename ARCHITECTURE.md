# Architecture

`hashit` is a single Rust binary. Each subcommand is a module; shared state
lives in the manifest, scan, and hashing layers.

## Module map

| Module | Responsibility |
|--------|----------------|
| `main.rs` | clap CLI: command/arg definitions, dispatch, helpers (`build_options`, `parse_excludes`, `set_workers`, `fileop_scan_opts`). |
| `manifest.rs` | The `.hashit` data model and shared utilities. |
| `hash.rs` | `HashAlgo` (Blake3/Sha256) and streaming `hash_file`. |
| `scan.rs` | The scan engine: `process_dir` (per-directory reconciliation) and `scan` (whole-tree, parallel). |
| `inventory.rs` | Aggregate all manifests into a JSON/CSV report. |
| `watch.rs` | Debounced recursive filesystem watcher. |
| `dedup.rs` | Duplicate detection and removal (interactive/auto). |
| `diff.rs` | Content-hash set comparison between two trees. |
| `sync.rs` | Copy content missing between two trees. |
| `fileops.rs` | Hash-aware `cp`/`mv`/`rm`. |
| `store.rs` | Global metadata index (SQLite) + thumbnail cache. *(`extract` feature)* |
| `drive.rs` | Per-drive `.hashit-drive` id marker and online/offline presence. *(`extract` feature)* |
| `extract/` | Extraction engine: file-type detection, EXIF (`exiftool_rs`), thumbnails (`magick-*`). *(`extract` feature)* |
| `index.rs` | Orchestrates scan → store reconciliation → extract-once-per-hash; `Indexer` for incremental watch updates. *(`extract` feature)* |
| `api.rs` | Typed, transport-agnostic logical-FS operations over the store (the API contract). *(`serve` feature)* |
| `serve.rs` | Thin axum HTTP layer exposing `api.rs` as a headless JSON API. *(`serve` feature)* |

## Data model (`manifest.rs`)

- `Manifest { version, updated_at, files: BTreeMap<String, FileEntry> }` — one
  per directory, named `.hashit`. `BTreeMap` keeps output deterministic.
- `FileEntry { size, mtime_ns, hash, algo, flags, hashed_at }`. Derives `Eq` so a
  rebuilt manifest can be compared to the old one to **skip writes on no-op
  scans** (avoids mtime churn).
- Atomic save: write to `.hashit.tmp.<pid>` then `rename`.
- Shared helpers used across commands:
  - `mtime_ns` / `set_mtime` — read/preserve modified time.
  - `ManifestCache` — lazily loads a source dir's manifest so `cp`/`mv`/`sync`
    can carry a file's existing entry without re-hashing.
  - `is_manifest_file` — recognizes `.hashit` and `.hashit.tmp.*`.

Metadata predicates also live in `scan.rs`: `is_apple_double` (`._*`) and
`is_dedup_pointer` (`*.dedup`). All three classes are excluded from hashing,
inventory, and watch reprocessing.

## Scan engine (`scan.rs`)

`process_dir(dir, opts, root)` is the single source of truth for keeping one
directory's manifest correct, and is reused by every command:

1. Load the existing manifest (map of name → entry).
2. List direct files (skip subdirs, metadata, and — by default — symlinks).
3. Apply the recompute heuristic per file: rehash if new / size changed /
   size-same-but-mtime-changed / algo changed; otherwise reuse the stored hash.
4. Hash the pending files **in parallel** (rayon) within the directory.
5. Drop entries for files no longer present.
6. Write only if the manifest actually changed; delete it if the directory has
   no files left.

`scan(root, opts)` collects every directory under `root` (pruning excluded
subtrees) and runs `process_dir` across them **in parallel**. `ScanStats`
accumulates new/modified/unchanged/removed counts via a reduce.

Concurrency model: directories are processed in parallel, and files within a
directory are hashed in parallel — good for both wide trees and large
directories. (A single directory of many huge files hashes within that dir's
pass; acceptable for current use.)

## Command flows

- **inventory** — walk `.hashit` files, flatten entries into sorted
  `InventoryRecord`s, render JSON or CSV.
- **watch** — initial `scan`, then a `notify` debounced watcher; each event
  batch maps to a set of affected directories that are re-run through
  `process_dir`. Own writes (`.hashit`, `._*`) are filtered to avoid feedback
  loops.
- **dedup** — `scan`, build `(algo,hash) → files`, and for each duplicate set
  pick a keeper (auto rank: non-hidden → fewest `/` → alphabetical; or
  interactive). Removed files get a relative `<file>.dedup` pointer; affected
  dirs are reconciled.
- **diff** — `scan` both sides (to stderr), build a `HashIndex` per side
  (`collect`), compute only-in-A / only-in-B / common, render in the chosen
  format. `--no-scan` compares existing manifests as-is.
- **sync** — `scan` both, index both, copy files whose hash is absent from the
  target at the source-relative path (`_N` suffix on collision). The source's
  manifest entry is **carried** to the target (hash kept; size/mtime re-read so
  the entry matches the stored file). Affected dirs are seeded then reconciled.
- **cp/mv/rm** (`fileops.rs`) — `scan_sources` first (directories recursively,
  file parents via `process_dir`). `cp`/`mv` carry source entries into target
  manifests; `mv` of a directory uses `rename` (its `.hashit` travels along),
  falling back to copy-tree + remove across filesystems; `rm` drops the source
  entry. `finalize` seeds carried entries and reconciles affected dirs with
  `process_dir` (reuses carried hashes, hashes only files lacking an entry).

## Metadata index (optional `extract` feature)

A second, **derived** layer sits on top of the manifests: a global, drive-aware
index for rich metadata, keyed by content hash rather than path.

- **Store (`store.rs`)** — one SQLite database at `~/.hashit/index.db`
  (`$HASHIT_HOME` overrides; WAL + a `schema_meta` version row for migrations)
  with a sibling `cache/` for thumbnails. Tables: `drives`, `content` (one row
  per `(algo, hash)`), `metadata` (EAV: `key`/`value`, indexed for reverse
  lookups), `locations` (one row per `(algo, hash, drive, path)`), `tags`
  (user tags/favorites, keyed by hash), and `link_members` (hash → link group).
  Everything except user tags/links is rebuildable from the `.hashit` manifests,
  so corruption is recovered by re-indexing; tags and links are user-authored
  state and key by content hash, so they apply to every copy.
- **Drives (`drive.rs`)** — each root carries a `.hashit-drive` marker (a UUID +
  label, written atomically like a manifest). The UUID is the stable `drive_id`;
  it travels with the drive (works on exFAT, unlike volume UUIDs). Presence is
  probed by checking the marker at a drive's last known root, so content can be
  flagged offline or a drive permanently detached (purging its locations and any
  now-orphaned content/metadata/thumbnails).
- **Extraction (`extract/`)** — `extract_all` identifies a file from a header
  read (`exiftool_rs::filetype`), and for images pulls EXIF (`exiftool_rs`) and a
  downscaled JPEG thumbnail (`magick-codecs`/`magick-core`). Designed as
  swappable free functions so an external-tool fallback can be added later.
- **Orchestration (`index.rs`)** — `index` scans, then walks the manifests
  (reusing `inventory::build_inventory`) and reconciles the store: a hash new to
  the store is extracted **once** (in parallel), and every file's location is
  upserted; stale locations on the drive are pruned. `Indexer::reconcile_dir`
  does the same for a single directory and is driven by `watch --index` via a
  callback, so live filesystem activity updates the index incrementally.

- **Tags & links (`store.rs`, CLI in `main.rs`)** — `tag`/`fav` attach
  hash-keyed user tags (favorites are the reserved `favorite` tag); `link` groups
  related hashes (e.g. a JPG + its RAW), with `link --auto` detecting same-stem,
  different-extension sidecars via `index::auto_link_groups`. Linking overlapping
  sets merges their groups; unlinking down to one member dissolves the group.
  Both are index-only (never written back to the original files).

The core `scan`/manifest path is untouched by all of this — the index is
additive and entirely behind the `extract` feature, which also gates its heavier
dependencies (SQLite, the metadata/image crates).

## Headless API (optional `serve` feature)

`hashit serve` exposes the index as a read-only logical filesystem over HTTP.
hashit ships no UI; an external web app is meant to be built on top.

- **`api.rs`** is the contract: typed, transport-agnostic operations over the
  store — `drives`, `list_dir`/`stat` (a per-drive directory tree synthesized
  from `locations.path`), `query`, `content_source`, `detail`, and `thumb`
  (generate-on-demand). A Rust consumer can call these directly.
- **`serve.rs`** is a thin axum layer: `/v1/{drives,ls,stat,query,content/:hash,
  content/:hash/meta,thumb/:hash}`. It binds localhost, guards requests with a
  bearer token (header or `?token=`), and sets permissive CORS so a browser app
  on another origin can call it. Each request opens the index on a blocking
  thread (SQLite WAL → concurrent readers), so no connection is held across an
  `.await`. Directory listings use a path-prefix **range scan**
  (`path >= prefix AND path < prefix‖+1`) plus SQL `substr`/`instr` to compute
  immediate children — no full-subtree materialization.

  Mutating endpoints (tag/favorite, link/unlink, and dedup "keep this") are
  opt-in via `--allow-write` (else `403`) and route through the same `api.rs`
  ops, reusing `dedup::remove_one` + `scan::process_dir` so deletions update the
  `.hashit` manifests and the index together; the dedup call requires an explicit
  `confirm` and skips offline copies.

The `serve` feature implies `extract` and adds the async HTTP stack
(`tokio`, `axum`, `tower-http`); the default build does not include it.

## Key invariants

- A `.hashit` describes only the files directly in its directory; recursion is
  implicit via one manifest per directory. Parent manifests never list subdirs,
  so moving/removing a whole directory needs no parent-manifest update.
- Carried entries always re-read the on-disk size/mtime so a later scan sees the
  file as `unchanged` and does not re-hash — verified to hold even on exFAT,
  whose coarse mtime resolution would otherwise force a rehash.
- Hash grouping is by `(algo, hash)` so different algorithms never collide.

## Dependencies

`clap` (CLI), `serde`/`serde_json` (manifest + JSON), `blake3` + `sha2`/`hex`
(hashing), `walkdir` (traversal), `notify` + `notify-debouncer-full` (watch),
`rayon` (parallel hashing), `glob` (excludes), `csv`, `chrono` (timestamps),
`anyhow` (errors).

Behind the optional `extract` feature: `rusqlite` (bundled SQLite), `uuid` (drive
ids), and the local `exiftool_rs` + `magick-core`/`magick-codecs` crates
(metadata + thumbnails).
