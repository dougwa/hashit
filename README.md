# hashit

A fast, cross-platform CLI for inventorying files by content hash. `hashit`
maintains a small `.hashit` manifest in every directory it scans, recording each
file's size, modified time, and BLAKE3 (or SHA-256) hash. From those manifests it
can produce inventories, watch for changes in real time, find and remove
duplicates, diff two trees by content, sync missing files between trees, and
perform hash-aware `cp`/`mv`/`rm`.

Built in Rust. Hashing is parallel; the binary is self-contained.

## Build

```sh
cargo build --release
# binary at target/release/hashit
```

## How it works

Each directory gets its own `.hashit` file (JSON) listing the files **directly**
in that directory:

```json
{
  "version": 1,
  "updated_at": "2026-05-29T14:07:50.265Z",
  "files": {
    "photo.jpg": {
      "size": 1048576,
      "mtime_ns": 1780063669760562061,
      "hash": "a1c3d13f…",
      "algo": "blake3",
      "flags": ["executable"],
      "hashed_at": "2026-05-29T14:07:50.265Z"
    }
  }
}
```

**Recompute heuristic** — hashing is expensive, so a file is only re-hashed when:
its size changed, or its size is unchanged but the modified time changed (or the
hash algorithm changed, or it's new). Otherwise the stored hash is reused. A
re-scan of an unchanged tree writes nothing.

**Managed/metadata files are skipped** and never inventoried: `.hashit` (and its
`.hashit.tmp.*` temp files), `*.dedup` pointers, and macOS AppleDouble `._*`
sidecars (use `--include-apple-double` to include the latter). Symlinks are
skipped unless `--follow-symlinks` is given.

## Commands

### scan — build/update manifests

```sh
hashit scan <path>... [--hash blake3|sha256] [--workers N] [--exclude GLOB]...
                      [--follow-symlinks] [--include-apple-double]
                      [--status] [-v|--verbose] [-q|--quiet] [--dry-run]
```

Traverses each `path`, updating every directory's `.hashit`. Removes entries for
files that no longer exist. `--status` prints a `new`/`modified`/`unchanged`/`removed`
line per file; `--dry-run` reports changes without writing. Pass several paths to
scan multiple roots in one run (the summary is combined).

### inventory — one aggregated report

```sh
hashit inventory <path>... [--format json|csv] [-o|--output FILE]
```

Walks all `.hashit` files under each `path` and emits a single sorted report
(relative path, hash, flags, size, timestamps) to stdout or a file. With multiple
paths the records are merged into one report.

### watch — real-time updates

```sh
hashit watch <path>... [scan options] [--debounce-ms 500]
```

Runs a scan, then watches each path recursively and updates `.hashit` files as
files are added, changed, or removed.

### dedup — remove duplicate content

```sh
hashit dedup <path>... (-i|--interactive | -a|--auto) [--no-dedup-link] [--dry-run]
```

Scans, groups files by hash, then resolves each duplicate set. Pass several paths
to find duplicates **across** roots (e.g. between two drives); matches are then
shown by absolute path.
- `-a/--auto` keeps the best file: **non-hidden first, then fewest `/`, then
  alphabetical**, removing the rest.
- `-i/--interactive` prompts per set: a number to keep, `s` to skip, `a` to
  switch to auto for the rest.
- Each removed duplicate leaves a `<file>.dedup` pointer containing the path to
  the kept file, relative to the removed file's directory (suppress with
  `--no-dedup-link`).

### diff — compare two trees by content

```sh
hashit diff <path1> <path2> [--format grouped|unified|json|summary]
                            [--show-common] [--no-scan]
```

Reports which content hashes are unique to each side (paths don't matter, so a
renamed/moved file counts as common). Default format is `grouped`. Scan progress
goes to stderr so `--format json` pipes cleanly.

### sync — copy missing files between trees

```sh
hashit sync <path1> <path2> [-d|--direction to|from|both] [--no-scan] [--dry-run]
```

Copies files whose content is missing from the target, placing each at its
source-relative path. `to` (default) copies path1→path2, `from` copies
path2→path1, `both` copies each way. Name collisions get a `_N` suffix (never
overwrites). The source's `.hashit` details are carried to the target so copied
content isn't re-hashed.

### cp / mv / rm — hash-aware file operations

```sh
hashit cp [-r] [-f] <src>... <dest>
hashit mv [-r] [-f] <src>... <dest>
hashit rm [-r] [-f] <path>...
```

Patterned after their POSIX cousins. `-r` recurses into directories (required by
`cp`/`rm`; `mv` moves directories regardless). `-f` overwrites an existing target
(`cp`/`mv`) or ignores missing files (`rm`). Sources are always scanned first.
- `cp`/`mv` carry the source's hash into the target's `.hashit` (no re-hash).
- `mv` of a directory uses `rename`, so its `.hashit` files travel with it.
- `rm` removes the entry from the source folder's `.hashit`.

## Common options

| Flag | Meaning |
|------|---------|
| `--hash blake3\|sha256` | Hash algorithm (default `blake3`) |
| `--workers N` | Hashing threads (`0` = number of CPUs) |
| `--exclude GLOB` | Exclude matching files/dirs (repeatable) |
| `--follow-symlinks` | Follow symlinks instead of skipping |
| `--include-apple-double` | Include macOS `._*` sidecars |
| `-q/--quiet` | Suppress per-file/progress output |

## Notes

- **macOS + exFAT/FAT/SMB**: the OS writes AppleDouble `._*` sidecars for files
  with extended attributes. `hashit` skips them by default.
- Copies/moves preserve modified time best-effort; on filesystems with coarse
  mtime resolution (e.g. exFAT) the stored time is re-read so manifests stay
  consistent.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the internal design.
