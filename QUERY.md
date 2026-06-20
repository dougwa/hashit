# Query language

The Search service (`hashit-idx`) accepts a single query **string** and returns
matching files. The language is a flat list of space-separated terms; a file
matches the query when it satisfies every term (terms are AND-combined). Each
term targets a field, optionally with a presence prefix, and tests either a
single value or a range.

```ebnf
QueryString  = Term (" " Term)*
Term         = Prefix? (Field Op)? Value
Prefix       = "+"        ; must match   (default)
             | "-"        ; must NOT match
Field        = word       ; property to search; omitted = the default field set
Op           = ":"        ; partial match (default)
             | "="        ; exact match
             | ":="       ; SQL LIKE glob (you supply the wildcards)
Value        = SingleValue | RangeValue
RangeValue   = SingleValue ".." SingleValue
             | SingleValue ".."              ; open upper bound  (≥ lower)
             | ".." SingleValue              ; open lower bound  (< upper)
SingleValue  = word | number | SingleQuoted | DoubleQuoted | DateExpression
```

A term with no `Prefix` is required (same as `+`). `-` negates: the file must
not match that term.

## Fields

If a term omits `Field:`, the value is matched against the **default field set**
— currently `name`, `path`, `hash` — and the term matches if *any* of them
match. The set is intended to be cheap to extend.

Field resolution is: known column first, otherwise the name is treated as a
metadata key (an extracted `tag` or user `property`). Metadata keys are matched
**case-insensitively**.

A metadata key may contain characters that the bare `field:` syntax can't hold —
notably the `:` in EXIF keys like `EXIF:Model`. Two ways to write them:

- **Quote the field name**: `"EXIF:Model":Canon` — the quoted name is taken
  literally (same escaping as quoted values).
- **`-` as an alias for `:`** in a bare name: `exif-model:Canon` resolves to the
  `exif:model` key, which matches `EXIF:Model` case-insensitively.

Quote the name when you need a literal `-` in a key (`"my-prop":x`).

| Field | Backing column | Value kind | Notes |
|-------|----------------|-----------|-------|
| `name` | `files.name` | text | basename; substring match |
| `path` | `files.path` | text | full path; substring match |
| `dir` | `files.dir` | text | containing directory (path minus the basename) |
| `hash` | `files.hash` | text | exact or prefix |
| `algo` | `files.algo` | text | e.g. `blake3`, `sha256` |
| `size` | `files.size` | number | bytes; supports ranges and size suffixes (`k`, `m`, `g`) |
| `mtime` | `files.mtime_ns` | **date** | file modification time; the default date field |
| `ext` | `content.ext` | text | extension without the dot |
| `file_type` | `content.file_type` | text | detected type |
| *anything else* | `meta_kv` | text | matches an extracted tag or user property key |

## Operators

The operator between a field and its value chooses how text is matched. All
text matching is case-insensitive.

| Operator | Meaning | Example |
|----------|---------|---------|
| `:` | partial match — substring (the default; `hash:` is a *prefix*) | `name:report` |
| `=` | exact — the whole value must be equal | `dir=/photos/2024` |
| `:=` | SQL `LIKE` glob — **you** supply the wildcards (`%` = any run, `_` = any char) | `dir:=/photos/%` |

An operator only applies when a `Field` precedes it, so a bare value containing
`=` or `:` is searched literally only when quoted (`"a=b"`); unquoted, the part
before the operator is read as a field name (`a=b` → field `a`, exact `b`), the
same way `:` has always behaved.

`=` and `:=` apply to every text field and to metadata values. They are not yet
supported on the ordered fields (`size`, `mtime`), which use ranges instead.

### Directories

`dir` is the file's containing directory. With the operators it covers both
folder use cases:

```text
dir=/photos/2024        files directly in /photos/2024 (not its subfolders)
dir:=/photos/2024/%     everything recursively under /photos/2024
dir:2024                any directory whose path contains "2024"
```

`hash` matches as an exact value or a prefix. (These per-field match modes are
deliberately a small, swappable table so new fields slot in without touching the
grammar.)

## Values and ranges

- **word** — an unquoted run of non-space, non-`:` characters.
- **number** — for `size`, an integer with an optional binary suffix:
  `1m..` is "≥ 1 MiB".
- **quoted string** — single or double quoted. The quote character and the
  backslash are escaped with a backslash: `name:"a \"quoted\" name"`,
  `name:'it\'s'`.
- **range** — `A..B` is inclusive of the low bound and exclusive of the high
  bound (`[A, B)`); either side may be omitted for an open range. Ranges apply
  to ordered fields (`size`, `mtime`).

```text
report                       default fields contain "report"
name:report -ext:tmp         name has "report", extension is not tmp
+hash:1afa                   hash begins with 1afa
size:1m..                    at least 1 MiB
size:..4k                    smaller than 4 KiB
camera:'Canon EOS'           tag/property "camera" contains "Canon EOS"
```

## Dates: `DateExpression`

Dates are written inside braces and target a date field (`mtime` today; a
future `taken`/`created` field would use the same syntax). The brace delimiters
mean a date can contain spaces and a leading `-` without colliding with the term
`Prefix` or with the `..` range operator.

```ebnf
DateExpression = "{" (RelativeDate | AbsoluteDate) "}"
```

### Everything is a half-open interval `[start, end)`

A date expression does not denote an instant; it denotes the interval implied by
its precision. This is what makes partial dates and ranges compose without
off-by-one errors. A term compiles to the index's `min`/`max` bounds on the date
column:

| Form | Compiles to |
|------|-------------|
| `mtime:{D}` | `start(D) ≤ mtime < end(D)` |
| `mtime:{A}..{B}` | `start(A) ≤ mtime < end(B)` |
| `mtime:{A}..` | `mtime ≥ start(A)` |
| `mtime:..{B}` | `mtime < end(B)` |

In a range the **low** bound contributes its `start` and the **high** bound its
`end`, so `mtime:{2024-01}..{2024-06}` spans Jan 1 through the end of June.

### AbsoluteDate

A truncated ISO-8601 timestamp. Precision determines the interval width. Bare
dates are interpreted in the **machine's local time zone**; append `Z` or an
explicit offset to override.

```ebnf
AbsoluteDate = Year ("-" Month ("-" Day ("T" Hour (":" Min (":" Sec)?)?)?)?)? Tz?
Tz           = "Z" | ("+" | "-") Hour ":" Min
```

| Literal | `[start, end)` |
|---------|----------------|
| `{2024}` | 2024-01-01 .. 2025-01-01 (local) |
| `{2024-03}` | all of March 2024 |
| `{2024-03-15}` | that local day |
| `{2024-03-15T14:30}` | that minute |
| `{2024-03-15Z}` | that day in **UTC** |
| `{2024-03-15+09:00}` | that day at offset +09:00 |

### RelativeDate

Anchored at `now`, in two registers.

```ebnf
RelativeDate = Offset | Keyword
Offset       = ("+" | "-") Number Unit                 ; sign REQUIRED
Unit         = "s" | "m" | "h" | "d" | "w" | "mo" | "y"
Keyword      = "now" | "today" | "yesterday" | "tomorrow"
             | ("this" | "last" | "next") " " UnitWord
```

`m` is **minute** and `mo` is **month** — distinct tokens, so there is no
case-sensitivity to remember.

**Offsets are points** (an instant at `now ± n units`). A bare offset term is the
window between that instant and `now`, with the sign choosing the side:

| Literal | `[start, end)` |
|---------|----------------|
| `{-7d}` | `now-7d` .. `now`  (rolling last 7 days) |
| `{-24h}..` | `mtime ≥ now-24h` |
| `{-3mo}..{-1mo}` | `now-3mo` .. `now-1mo` |
| `{now}` | the instant `now` (a bound; degenerate on its own) |

**Keywords are calendar intervals**, snapped to local boundaries:

| Literal | `[start, end)` |
|---------|----------------|
| `{today}` | local midnight .. next local midnight |
| `{yesterday}` | the prior local day |
| `{last month}` | the previous calendar month |
| `{this year}` | Jan 1 .. next Jan 1, local |

So `{-1d}` (a rolling 24-hour window ending now) and `{yesterday}` (the calendar
day) are intentionally different. Week boundaries (`{this week}`, `{last week}`)
follow **ISO** rules — weeks start on Monday — in local time.

### Disambiguation inside `{…}`

A single lookahead character selects the alternative:

- leading **digit** → `AbsoluteDate` (a year)
- leading **`+` / `-`** → relative `Offset`
- leading **letter** → relative `Keyword`

Requiring an explicit sign on offsets is what keeps `{2024}` (the year) from
being read as a duration.

## Examples

```text
photos +ext:jpg mtime:{last month}        jpgs named …photos…, modified last month
mtime:{2024}..{2025} -path:backup          modified in 2024, not under a backup path
size:100m.. mtime:{-7d}                     ≥ 100 MiB and touched in the last 7 days
file_type:image mtime:..{2020}             images last modified before 2020
camera:'Canon' mtime:{2023-06}..{2023-08}  Canon shots from summer 2023
```

## Implementation

Lives in `crates/hashit-idx/src/query/`:

- **`ast.rs`** — the parsed `Query` / `Term` / `Field` / `Op` / `Matcher` / `Value`.
- **`date.rs`** — `DateExpr` resolves against a captured `now` into an
  `Interval { start, end }` of nanoseconds since the Unix epoch (the unit of
  `files.mtime_ns`); `end` is `None` for an open upper bound. The bare-term vs.
  range-bound distinction for offsets lives in `resolve` / `start` / `end`.
- **`parse.rs`** — a hand-written scanner (`parse`) and the `{…}` date parser
  (`parse_date`); `resolve_field` is the extensible field table.
- **`lower.rs`** — `lower(Query, now) -> Filter`, a flat AND of independently
  negatable `Pred`s (`Text`, `AnyText`, `Range`, `Meta`). `Op` lowers to a
  `TextMode` (`Substring`/`Prefix`/`Exact`/`Like`) on the text and metadata-value
  predicates.

The Search service parses the string **server-side**: `QueryRequest` carries a
`string query` field (number 10) that, when non-empty, supersedes the structured
fields. `store::query_filter` turns the `Filter` into one SQL `WHERE` (each
`Pred` → an ANDed fragment; `Meta` → an `EXISTS` over `meta_kv`; `AnyText` → an
`OR` over the default columns; `negate` → `NOT (...)`, with `COALESCE` so a
missing content row reads as empty rather than poisoning a negated match).
