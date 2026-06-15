use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use walkdir::WalkDir;

use crate::manifest::{ns_to_rfc3339, Manifest, MANIFEST_NAME};
use crate::scan::ScanOptions;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Json,
    Csv,
    /// Static HTML page: a filterable table aggregated by hash.
    Html,
}

/// One row of the HTML report: all files sharing a single content hash.
#[derive(Debug, Serialize)]
struct HashGroup {
    /// Short, display-friendly prefix of the hash.
    short: String,
    /// Full hash (used for the cell's tooltip / exact filtering).
    hash: String,
    algo: String,
    /// Every path that resolved to this hash, sorted.
    paths: Vec<String>,
    /// Number of files (copies) with this hash.
    count: usize,
    /// File size in bytes (identical across copies of the same content).
    size: u64,
    /// Earliest modified time across the copies, as nanoseconds since epoch.
    earliest_mtime_ns: u64,
    /// Earliest modified time, RFC3339 (empty if unavailable).
    earliest_mtime: String,
}

/// Group inventory records by hash, picking the earliest modified time as the
/// group's date. Groups are sorted by descending count, then path.
fn aggregate_by_hash(records: &[InventoryRecord]) -> Vec<HashGroup> {
    use std::collections::BTreeMap;
    // Keyed by hash; preserves a stable order before the final sort.
    let mut groups: BTreeMap<&str, HashGroup> = BTreeMap::new();
    for r in records {
        let g = groups.entry(&r.hash).or_insert_with(|| HashGroup {
            short: short_hash(&r.hash),
            hash: r.hash.clone(),
            algo: r.algo.clone(),
            paths: Vec::new(),
            count: 0,
            size: r.size,
            earliest_mtime_ns: u64::MAX,
            earliest_mtime: String::new(),
        });
        g.paths.push(r.path.clone());
        g.count += 1;
        // Treat 0 (unavailable mtime) as "no data" so it doesn't win the min.
        if r.mtime_ns != 0 && r.mtime_ns < g.earliest_mtime_ns {
            g.earliest_mtime_ns = r.mtime_ns;
        }
    }
    let mut out: Vec<HashGroup> = groups
        .into_values()
        .map(|mut g| {
            g.paths.sort();
            if g.earliest_mtime_ns == u64::MAX {
                g.earliest_mtime_ns = 0;
            } else {
                g.earliest_mtime = ns_to_rfc3339(g.earliest_mtime_ns);
            }
            g
        })
        .collect();
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.paths.first().cmp(&b.paths.first()))
    });
    out
}

/// First 12 characters of a hash (or the whole hash if shorter).
fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[derive(Debug, Serialize)]
pub struct InventoryRecord {
    /// Path relative to the scan root, using forward slashes.
    pub path: String,
    pub hash: String,
    pub algo: String,
    /// Flags joined with '|' (CSV-friendly).
    pub flags: String,
    pub size: u64,
    pub mtime_ns: u64,
    pub mtime: String,
    pub hashed_at: String,
}

/// Normalize a path to forward-slash form for portable output.
fn to_slash(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        if let Component::Normal(s) = c {
            parts.push(s.to_string_lossy().to_string());
        }
    }
    parts.join("/")
}

/// Walk every `.hashit` under `root` and aggregate entries into a sorted
/// report. Directories matching the ignore/exclude rules are pruned (their
/// manifests are never read), and individual entries that match are dropped,
/// so the report mirrors what a scan with the same options would record.
pub fn build_inventory(root: &Path, opts: &ScanOptions) -> Result<Vec<InventoryRecord>> {
    let mut records: Vec<InventoryRecord> = Vec::new();
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        // Mirror the scan walk: never prune the root, but skip ignored/excluded
        // subtrees so stale manifests left inside them aren't aggregated.
        e.depth() == 0 || !(e.file_type().is_dir() && opts.is_skipped(e.path(), root))
    });
    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() || entry.file_name() != MANIFEST_NAME {
            continue;
        }
        let dir = entry.path().parent().unwrap_or(root);
        let rel_dir = dir.strip_prefix(root).unwrap_or(Path::new(""));
        let manifest = match Manifest::load(dir)? {
            Some(m) => m,
            None => continue,
        };
        for (name, fe) in manifest.files {
            let mut full: PathBuf = rel_dir.to_path_buf();
            full.push(&name);
            // Drop entries that match ignore/exclude (e.g. files recorded by an
            // earlier scan run without these options).
            if opts.is_skipped(&dir.join(&name), root) {
                continue;
            }
            records.push(InventoryRecord {
                path: to_slash(&full),
                hash: fe.hash,
                algo: fe.algo,
                flags: fe.flags.join("|"),
                size: fe.size,
                mtime_ns: fe.mtime_ns,
                mtime: ns_to_rfc3339(fe.mtime_ns),
                hashed_at: fe.hashed_at,
            });
        }
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(records)
}

/// Render the inventory to stdout or a file in the requested format.
pub fn write_inventory(
    records: &[InventoryRecord],
    format: Format,
    out: Option<&Path>,
) -> Result<()> {
    let mut sink: Box<dyn Write> = match out {
        Some(p) => Box::new(File::create(p).with_context(|| format!("creating {}", p.display()))?),
        None => Box::new(io::stdout().lock()),
    };
    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(records).context("serializing inventory")?;
            sink.write_all(json.as_bytes())?;
            sink.write_all(b"\n")?;
        }
        Format::Csv => {
            let mut wtr = csv::Writer::from_writer(&mut sink);
            for r in records {
                wtr.serialize(r).context("writing csv row")?;
            }
            wtr.flush()?;
        }
        Format::Html => {
            let groups = aggregate_by_hash(records);
            let html = render_html(&groups)?;
            sink.write_all(html.as_bytes())?;
        }
    }
    Ok(())
}

/// Render the hash-aggregated groups into a self-contained static HTML page.
///
/// The group data is embedded as JSON and the table is built client-side; all
/// cell text is set via `textContent`, so file paths can't inject markup.
fn render_html(groups: &[HashGroup]) -> Result<String> {
    let total_files: usize = groups.iter().map(|g| g.count).sum();
    // Escape "</" so a path containing "</script>" can't close the data block.
    let data = serde_json::to_string(groups)
        .context("serializing inventory")?
        .replace("</", "<\\/");
    Ok(HTML_TEMPLATE
        .replace("__DATA__", &data)
        .replace("__GROUPS__", &groups.len().to_string())
        .replace("__FILES__", &total_files.to_string())
        .replace("__GENERATED__", &crate::manifest::now_rfc3339()))
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>hashit inventory</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.4 system-ui, sans-serif; margin: 1.5rem; }
  h1 { font-size: 1.2rem; margin: 0 0 .25rem; }
  .meta { color: #888; margin-bottom: 1rem; }
  table { border-collapse: collapse; width: 100%; }
  th, td { text-align: left; padding: .35rem .5rem; border-bottom: 1px solid #8884; vertical-align: top; }
  th { position: sticky; top: 0; background: Canvas; cursor: pointer; user-select: none; white-space: nowrap; }
  th .arrow { color: #888; font-size: .8em; }
  thead tr.filters th { cursor: auto; }
  thead input { width: 100%; box-sizing: border-box; font: inherit; padding: .2rem .3rem; }
  td.hash { font-family: ui-monospace, monospace; white-space: nowrap; }
  td.paths { white-space: pre-line; word-break: break-all; }
  td.num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
  tr:hover td { background: #8881; }
  .empty { color: #888; padding: 1rem 0; }
</style>
</head>
<body>
<h1>hashit inventory</h1>
<div class="meta">__GROUPS__ unique hashes &middot; __FILES__ files &middot; generated __GENERATED__</div>
<table>
  <thead>
    <tr id="head">
      <th data-key="short" data-type="str">Hash</th>
      <th data-key="paths" data-type="str">Path(s)</th>
      <th data-key="count" data-type="num">Count</th>
      <th data-key="size" data-type="num">Size</th>
      <th data-key="earliest_mtime" data-type="str">Earliest date</th>
    </tr>
    <tr class="filters">
      <th><input data-col="short" placeholder="filter"></th>
      <th><input data-col="paths" placeholder="filter"></th>
      <th><input data-col="count" placeholder="filter"></th>
      <th><input data-col="size" placeholder="filter"></th>
      <th><input data-col="earliest_mtime" placeholder="filter"></th>
    </tr>
  </thead>
  <tbody id="rows"></tbody>
</table>
<div id="noresults" class="empty" hidden>No matching rows.</div>
<script id="data" type="application/json">__DATA__</script>
<script>
const DATA = JSON.parse(document.getElementById("data").textContent);
const tbody = document.getElementById("rows");
const noresults = document.getElementById("noresults");
let sortKey = "count", sortDir = -1;

function fmtSize(n) {
  const u = ["B","KB","MB","GB","TB","PB"]; let i = 0; n = Number(n);
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return (i ? n.toFixed(1) : n) + " " + u[i];
}
function dateText(s) { return s ? s.slice(0, 10) : ""; }

// Pre-compute the lowercased text used for filtering each column.
const rowsData = DATA.map(g => ({
  g,
  text: {
    short: (g.short + " " + g.hash).toLowerCase(),
    paths: g.paths.join("\n").toLowerCase(),
    count: String(g.count),
    size: (fmtSize(g.size) + " " + g.size).toLowerCase(),
    earliest_mtime: dateText(g.earliest_mtime).toLowerCase(),
  },
}));
const filters = {};
document.querySelectorAll("thead input").forEach(inp => {
  inp.addEventListener("input", () => {
    filters[inp.dataset.col] = inp.value.trim().toLowerCase();
    render();
  });
});
document.querySelectorAll("#head th").forEach(th => {
  th.addEventListener("click", () => {
    const k = th.dataset.key;
    if (sortKey === k) { sortDir = -sortDir; }
    else { sortKey = k; sortDir = th.dataset.type === "num" ? -1 : 1; }
    render();
  });
});

function render() {
  let rows = rowsData.filter(r =>
    Object.entries(filters).every(([col, q]) => !q || r.text[col].includes(q)));
  const numeric = document.querySelector(`#head th[data-key="${sortKey}"]`).dataset.type === "num";
  rows.sort((a, b) => {
    let av = a.g[sortKey], bv = b.g[sortKey];
    if (sortKey === "paths") { av = a.g.paths[0] || ""; bv = b.g.paths[0] || ""; }
    const c = numeric ? av - bv : String(av).localeCompare(String(bv));
    return c * sortDir;
  });
  tbody.replaceChildren();
  for (const { g } of rows) {
    const tr = document.createElement("tr");
    const cells = [
      ["hash", g.short, g.algo + ":" + g.hash],
      ["paths", g.paths.join("\n"), null],
      ["num", String(g.count), null],
      ["num", fmtSize(g.size), g.size + " bytes"],
      ["", dateText(g.earliest_mtime), g.earliest_mtime],
    ];
    for (const [cls, text, title] of cells) {
      const td = document.createElement("td");
      if (cls) td.className = cls;
      if (title) td.title = title;
      td.textContent = text;
      tr.appendChild(td);
    }
    tbody.appendChild(tr);
  }
  noresults.hidden = rows.length > 0;
  document.querySelectorAll("#head th").forEach(th => {
    const a = th.querySelector(".arrow");
    if (a) a.remove();
    if (th.dataset.key === sortKey) {
      const s = document.createElement("span");
      s.className = "arrow";
      s.textContent = sortDir > 0 ? " ▲" : " ▼";
      th.appendChild(s);
    }
  });
}
render();
</script>
</body>
</html>
"##;
