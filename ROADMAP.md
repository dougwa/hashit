# Roadmap

`hashit` is a **portable, per-directory file inventory** with a clean split of
responsibilities:

- **`hashit`** maintains the `.hashit` manifests and per-hash metadata files. It
  is the only writer and works strictly on individual files under its roots.
- **`hashit-idx`** provides the global view: a rebuildable SQLite index over the
  manifests and metadata files, exposed as read-only search over local gRPC.

The portable, on-disk artifacts (`.hashit` manifests and the sharded
`.hashit-meta` folder) are the source of truth; the index is derived and
disposable. A separate web-UI project is expected to sit on top of the gRPC
services. hashit itself ships no UI.

## Status

- **Workspace split** ✅
  Converted the single kitchen-sink binary into a cargo workspace:
  `hashit-core`, `hashit-extract`, `hashit`, `hashit-idx`. Removed the global
  SQLite store, drive registry, axum HTTP API, and the `inventory`/`dedup`/
  `diff`/`sync`/`tag`/`fav`/`link` commands from the scanner.

- **Scanner CLI** ✅
  `hashit` = `scan`, `watch`, `cp`, `mv`, `rm`, `get-meta`, `put-meta`.

- **File-based metadata** ✅
  `scan --meta-thumbnail/--meta-preview/--meta-tags/--meta-all` generate per-hash
  artifacts (thumbnail, preview, EXIF tags) into a single sharded `--meta-folder`
  (default `~/.hashit-meta`). `get-meta`/`put-meta` read and edit them by content
  hash. Links/favorites from the old model are dropped; user data is plain
  `properties` in `<hash>.meta.json`.

- **`hashit-idx` daemon** ✅
  Rebuildable SQLite index over `.hashit` + `*.meta.json`, kept fresh by a
  filesystem watcher, with a read-only gRPC Search service (name/hash/size/date/
  tag filters + stats), bound to localhost.

- **`hashit watch --serve`** ✅
  Localhost gRPC FileOps server (cp/mv/rm/get-meta/put-meta) running alongside the
  watcher, routed through the same core code as the CLI.

## Next / ideas

- **gRPC ergonomics** — optional server reflection so clients don't need the
  `.proto`; richer Search (sort options, facet counts, pagination cursors).
- **Index targets** — let `hashit-idx` auto-discover removable drives (currently
  it takes explicit roots), handling plug/unplug.
- **Broader extractors** — video metadata + thumbnails (optional `ffmpeg`
  delegate), more document/container types, audio. The `hashit-extract` seam is
  designed to accept these without changing callers.
- **Geocoding** — translate the already-extracted GPS tags into region/country/
  landmark properties (offline dataset by default, optional online provider).
- **Web UI** — a separate project consuming the two gRPC services for an
  interactive browse/search/dedup experience.
