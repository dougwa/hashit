//! Metadata extraction via `exiftool_rs`.

use std::path::Path;

use anyhow::Result;
use exiftool_rs::value::Value;

use crate::store::MetaTag;

/// Read all metadata tags from `path` and flatten them into EAV rows.
/// The human-readable PrintConv rendering is stored as the value; raw numeric
/// values fall back to a debug rendering when there is no print string.
pub fn extract_tags(path: &Path) -> Result<Vec<MetaTag>> {
    let tags = exiftool_rs::extract_from_path(path)?;
    let mut out = Vec::with_capacity(tags.len());
    for t in tags {
        let value = if !t.print.is_empty() {
            t.print
        } else {
            render_value(&t.value)
        };
        out.push(MetaTag {
            group: t.group0,
            key: t.name,
            value,
        });
    }
    Ok(out)
}

/// Fallback rendering for a raw value when no PrintConv string is present.
fn render_value(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::U(xs) => join(xs),
        Value::I(xs) => join(xs),
        Value::F(xs) => join(xs),
        Value::R(xs) => xs
            .iter()
            .map(|(n, d)| format!("{n}/{d}"))
            .collect::<Vec<_>>()
            .join(" "),
        Value::Bytes(b) => format!("[{} bytes]", b.len()),
    }
}

fn join<T: std::fmt::Display>(xs: &[T]) -> String {
    xs.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}
