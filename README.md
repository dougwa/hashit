# hashit

A fast, cross-platform toolset for inventorying files by content hash, split
into two focused binaries:

- **`hashit`** — the command-line scanner. It maintains a small `.hashit`
  manifest in every directory it touches (recording each file's size, modified
  time, and BLAKE3 or SHA-256 hash), does hash-aware `cp`/`mv`/`rm`, and can
  generate per-content metadata artifacts (thumbnails, previews, EXIF tags).
- **`hashit-idx`** — a read-only daemon that builds a global, searchable SQLite
  index from the `.hashit` manifests and metadata files, keeps it fresh by
  watching for changes, and serves search over a localhost-only gRPC endpoint.

The division of labour: `hashit` only ever works with individual files under the
roots you give it and is the **only** writer. `hashit-idx` provides the
**global view** (search across everything) and never modifies anything.

Built in Rust as a cargo workspace. Hashing is parallel; metadata extraction
runs once per content hash.

## Workspace layout

| Crate | Role |
|-------|------|
| `hashit-core` | Portable `.hashit` manifest model, hashing, scan engine, file ops, and the per-hash metadata file format. Dependency-light; shared by both binaries. |
| `hashit-extract` | Extraction engine: file-type detection, EXIF tags, thumbnails, previews (`exiftool_rs` + `magick-*`). Used only by `hashit`. |
| `hashit` | The scanner CLI binary. |
| `hashit-idx` | The index + gRPC search daemon binary. |

```sh
cargo build --release
# binaries at target/release/{hashit,hashit-idx}
```

## How it works

Each directory gets its own `.hashit` file (JSON) listing the files **directly**
in that directory:

```json
{
  "version": 1,
  "updated_at": "2026-06-19T14:07:50.265Z",
  "files": {
    "photo.jpg": {
      "size": 1048576,
      "mtime_ns": 1780063669760562061,
      "hash": "a1c3d13f…",
      "algo": "blake3",
      "flags": ["executable"],
      "hashed_at": "2026-06-19T14:07:50.265Z"
    }
  }
}
```

**Recompute heuristic** — hashing is expensive, so a file is only re-hashed
when: it's new, its size changed, its size is unchanged but the modified time
changed, or the hash algorithm changed. Otherwise the stored hash is reused. A
re-scan of an unchanged tree writes nothing.

**Managed/metadata files are skipped** and never hashed: `.hashit` (and its
`.hashit.tmp.*` temp files), `*.dedup` pointers, and macOS AppleDouble `._*`
sidecars (use `--include-apple-double` to include the latter). Symlinks are
skipped unless `--follow-symlinks` is given.

## Per-hash metadata

Metadata artifacts live as plain files in a single, shared folder
(`--meta-folder`, default `$HASHIT_META` or `~/.hashit-meta`), keyed by content
hash and sharded by hash prefix so one directory never holds millions of files:

```text
<meta-folder>/1a/fa/1afaef….meta.json      structured: EXIF tags + user properties
<meta-folder>/1a/fa/1afaef….thumbnail.jpg  512px thumbnail
<meta-folder>/1a/fa/1afaef….preview.jpg    2048px preview
```

Everything here is **derived** and rebuildable, except user-authored
`properties` (set via `put-meta`), which are the only state that can't be
recovered from the original files. Because metadata is keyed by content hash, it
is computed once per unique file and applies to every duplicate copy.

## `hashit` commands

### scan — build/update manifests

```sh
hashit scan <path>... [--hash blake3|sha256] [--workers N] [--exclude GLOB]...
                      [--follow-symlinks] [--include-apple-double]
                      [--status] [-v|--verbose] [-q|--quiet] [--dry-run]
                      [--meta-thumbnail] [--meta-preview] [--meta-tags] [--meta-all]
                      [--meta-folder PATH]
```

Traverses each `path`, updating every directory's `.hashit` and removing entries
for files that no longer exist. The `--meta-*` flags run an extract-once-per-hash
pass after the scan, generating any missing artifacts into the metadata folder
(`--meta-all` = thumbnail + preview + tags).

### watch — real-time updates

```sh
hashit watch <path>... [scan/meta options] [--debounce-ms 500] [--serve [ADDR]]
```

Runs a scan, then watches each path recursively and updates `.hashit` files (and,
with `--meta-*`, metadata artifacts) as files change. With `--serve` it also runs
a localhost-only gRPC **FileOps** server (default `127.0.0.1:50552`) exposing
`cp`/`mv`/`rm`/`get-meta`/`put-meta` — handy for a local UI to drive mutations.

### cp / mv / rm — hash-aware file operations

```sh
hashit cp [-r] [-f] <src>... <dest>
hashit mv [-r] [-f] <src>... <dest>
hashit rm [-r] [-f] <path>...
```

Patterned after their POSIX cousins. `-r` recurses into directories (required by
`cp`/`rm`; `mv` moves directories regardless). `-f` overwrites an existing target
(`cp`/`mv`) or ignores missing files (`rm`). `cp`/`mv` carry the source's hash
into the target's `.hashit` (no re-hash); `mv` of a directory uses `rename` so
its `.hashit` files travel with it.

### get-meta / put-meta — inspect and edit metadata

```sh
hashit get-meta <path>... [--meta-folder PATH]
hashit put-meta <path>... [--set KEY=VALUE]... [--remove KEY]... [--meta-folder PATH]
```

`get-meta` resolves each path to its content hash and prints (as JSON) the
extracted tags, user properties, and the thumbnail/preview paths if those
artifacts exist. `put-meta` sets or removes user properties. Both operate **by
content hash**, so an edit applies to every copy of that content.

## `hashit-idx` — global index + search

### serve — index, watch, and serve

```sh
hashit-idx serve <root>... [--meta-folder PATH] [--db PATH]
                           [--addr 127.0.0.1:50551] [--debounce-ms 500]
```

Builds a SQLite index (default `$HASHIT_HOME/index.db` or `~/.hashit/index.db`)
from the `.hashit` manifests under each root and the `*.meta.json` files in the
metadata folder, then watches both for changes to keep it current. The index is
fully rebuildable from disk and is never written to by anything but `hashit-idx`.

It serves a read-only **Search** gRPC service on localhost
(`proto/search.proto`):

- `Query` — by structured fields, or by a query-language `query` string (see
  [`QUERY.md`](QUERY.md)); paginated.
- `Stats` — index-wide counts (files, hashes, tag/property rows).

Because the service is gRPC, point any gRPC client at it. Example with
[`grpcurl`](https://github.com/fullstorydev/grpcurl) using the proto directly
(no server reflection needed):

```sh
grpcurl -plaintext -import-path proto -proto search.proto \
  -d '{"query":"sunset size:100k.."}' \
  127.0.0.1:50551 hashit.search.v1.Search/Query
```

### query — search the index from the CLI

```sh
hashit-idx query "<query>" [--db PATH] [--limit N] [--offset N] [--paths-only]
```

Runs a [query-language](QUERY.md) search directly against the existing index
(no running daemon required) and prints one line per match — modified time,
size, type, and path — or just paths with `--paths-only`. An empty query
matches everything. Examples:

```sh
hashit-idx query 'photos +ext:jpg mtime:{last month}'
hashit-idx query 'size:100m.. mtime:{-7d}..' --paths-only
hashit-idx query '"EXIF:Model":Canon' --limit 20
```

## gRPC services

Two localhost-only services, defined under `proto/`:

| Service | Served by | Purpose |
|---------|-----------|---------|
| `hashit.fileops.v1.FileOps` | `hashit watch --serve` | mutations: cp/mv/rm/get-meta/put-meta |
| `hashit.search.v1.Search` | `hashit-idx` | read-only search + stats |

## Common options

| Flag | Meaning |
|------|---------|
| `--hash blake3\|sha256` | Hash algorithm (default `blake3`) |
| `--workers N` | Hashing threads (`0` = number of CPUs) |
| `--exclude GLOB` | Exclude matching files/dirs (repeatable) |
| `--ignore STR` | Ignore paths containing a substring (repeatable, `$HASHIT_IGNORE`) |
| `--follow-symlinks` | Follow symlinks instead of skipping |
| `--include-apple-double` | Include macOS `._*` sidecars |
| `-q/--quiet` | Suppress per-file/progress output |

## Notes

- **macOS + exFAT/FAT/SMB**: the OS writes AppleDouble `._*` sidecars for files
  with extended attributes. `hashit` skips them by default.
- Copies/moves preserve modified time best-effort; on filesystems with coarse
  mtime resolution (e.g. exFAT) the stored time is re-read so manifests stay
  consistent.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the internal design and
[ROADMAP.md](ROADMAP.md) for status and what's next.
