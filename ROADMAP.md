# Roadmap

hashit is becoming a **logical file service over native filesystems**: the
per-directory `.hashit` manifests stay the portable source of truth, and a
derived, drive-aware global index (`~/.hashit/index.db`) adds metadata,
thumbnails, tags, links, and a headless HTTP API. The end state is **hashit-api**
— a low-level logical-FS API — with the user experience built as a **separate
web-UI project** that consumes the API. hashit itself ships no UI.

## Status

- **M1 — Metadata index foundation** ✅ (PR #4)
  Global SQLite index, drive registry (online/offline/detach), extract-once-per-hash
  (EXIF + file type via `exiftool_rs`), thumbnails (`magick-*`), `index`/`query`/
  `drive`/`thumb` CLI, `watch --index`. Behind the `extract` feature.

- **M2 — Tags, favorites, file linking** ✅ (PR #5)
  Hash-keyed user tags + favorites and logical file links (incl. `--auto` JPG+RAW
  sidecar detection). Index-only; `tag`/`fav`/`link`/`unlink`/`links` CLI; query
  `--tag`/`--favorite`.

- **M3 — Headless logical-FS HTTP API** ✅ (PR #6)
  Read-only `hashit serve` (`serve` feature): `/v1/{drives,ls,stat,query,
  content/:hash,content/:hash/meta,thumb/:hash}`. Localhost + bearer token,
  permissive CORS, `api.rs` typed contract + thin axum layer.

- **M3.5 — API mutations + dedup actions** ✅
  Write endpoints over the API, opt-in via `hashit serve --allow-write` (else
  `403`): tag/favorite and link/unlink edits, and dedup "keep this" — delete the
  other copies, leaving `.dedup` pointers like the CLI (requires `confirm:true`,
  skips offline copies). Routed through the same `api.rs` boundary so manifests
  and the index stay consistent.

- **Web-UI project** (separate repo, after the API is complete)
  A web front-end built with standard frameworks, consuming hashit-api to deliver
  the interactive browse/dedup experience. Not part of this repo.

## Deferred

- **M4 — Geocoding + more extractors** ⏸️ (resume later)
  - GPS → region/country/state and named landmarks (e.g. national parks). Plan:
    offline reverse-geocode dataset by default (works on disconnected drives),
    optional online provider for street-level / landmark detail. Raw GPS tags are
    already extracted and stored by M1, so this is a translation layer over
    existing data.
  - Broader extractors: video metadata + thumbnails (optional `ffmpeg` delegate),
    more document/container types, audio. The `Extractor`/thumbnail seam in
    `src/extract/` is designed to accept these without changing callers.
