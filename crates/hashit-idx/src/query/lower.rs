//! Lowering: [`Query`] + a captured `now` → [`Filter`], the input to the SQL
//! builder. Each term becomes one independently-negatable column predicate; the
//! builder ANDs them. This is the representation today's `QueryParams` can't hold
//! (it lacks negation and per-field multiplicity), so the Search service should
//! parse the query string into a `Filter` server-side.

use anyhow::{bail, Result};
use chrono::{DateTime, Local};

use super::ast::{Field, Matcher, Presence, Query, Value};

/// A flat AND of column predicates.
pub struct Filter {
    pub preds: Vec<Pred>,
    pub limit: u32,
    pub offset: u32,
}

pub enum Pred {
    /// Substring match on a text column (`LIKE %needle%`, negated → `NOT LIKE`).
    Text {
        col: TextCol,
        needle: String,
        negate: bool,
    },
    /// Substring match across the default field set (OR of the default columns).
    AnyText { needle: String, negate: bool },
    /// Half-open `[lo, hi)` on an ordered column; `None` is an open side.
    Range {
        col: NumCol,
        lo: Option<i64>,
        hi: Option<i64>,
        negate: bool,
    },
    /// A tag/property key, optionally constrained to a value (`EXISTS` in `meta_kv`).
    Meta {
        key: String,
        value: Option<String>,
        negate: bool,
    },
}

pub enum TextCol {
    Name,
    Path,
    Hash,
    Algo,
    Ext,
    FileType,
}

pub enum NumCol {
    Size,
    Mtime,
}

pub fn lower(q: &Query, now: DateTime<Local>) -> Result<Filter> {
    let mut preds = Vec::with_capacity(q.terms.len());
    for t in &q.terms {
        let negate = matches!(t.presence, Presence::MustNot);
        let pred = match (&t.field, &t.matcher) {
            // Ordered fields: ranges and single values both become `Range`.
            (Field::Mtime, m) => {
                let (lo, hi) = date_bounds(m, now)?;
                Pred::Range {
                    col: NumCol::Mtime,
                    lo,
                    hi,
                    negate,
                }
            }
            (Field::Size, m) => {
                let (lo, hi) = size_bounds(m)?;
                Pred::Range {
                    col: NumCol::Size,
                    lo,
                    hi,
                    negate,
                }
            }
            // Text columns.
            (Field::Name, Matcher::Single(Value::Text(s))) => text(TextCol::Name, s, negate),
            (Field::Path, Matcher::Single(Value::Text(s))) => text(TextCol::Path, s, negate),
            (Field::Hash, Matcher::Single(Value::Text(s))) => text(TextCol::Hash, s, negate),
            (Field::Algo, Matcher::Single(Value::Text(s))) => text(TextCol::Algo, s, negate),
            (Field::Ext, Matcher::Single(Value::Text(s))) => text(TextCol::Ext, s, negate),
            (Field::FileType, Matcher::Single(Value::Text(s))) => {
                text(TextCol::FileType, s, negate)
            }
            // The default field set.
            (Field::Default, Matcher::Single(Value::Text(s))) => Pred::AnyText {
                needle: s.clone(),
                negate,
            },
            // Unknown field name → a tag/property key.
            (Field::Meta(k), Matcher::Single(v)) => Pred::Meta {
                key: k.clone(),
                value: Some(v.as_text()),
                negate,
            },
            // Ranges are only meaningful on ordered fields.
            (f, Matcher::Range(..)) => bail!("field {f:?} does not support ranges"),
            (f, _) => bail!("unsupported value for field {f:?}"),
        };
        preds.push(pred);
    }
    Ok(Filter {
        preds,
        limit: q.limit,
        offset: q.offset,
    })
}

fn text(col: TextCol, needle: &str, negate: bool) -> Pred {
    Pred::Text {
        col,
        needle: needle.to_string(),
        negate,
    }
}

/// Low bound contributes `start`, high bound `end`; either may be open.
fn date_bounds(m: &Matcher, now: DateTime<Local>) -> Result<(Option<i64>, Option<i64>)> {
    match m {
        Matcher::Single(Value::Date(d)) => {
            let iv = d.resolve(now);
            Ok((iv.start, iv.end))
        }
        Matcher::Range(lo, hi) => Ok((
            date_bound(lo.as_ref(), now, true)?,
            date_bound(hi.as_ref(), now, false)?,
        )),
        _ => bail!("mtime expects a date {{…}}"),
    }
}

fn date_bound(v: Option<&Value>, now: DateTime<Local>, low: bool) -> Result<Option<i64>> {
    match v {
        None => Ok(None),
        Some(Value::Date(d)) => Ok(if low { d.start(now) } else { d.end(now) }),
        Some(_) => bail!("mtime bound must be a date {{…}}"),
    }
}

/// `[A, B)` for `size`; a single value `n` becomes the unit interval `[n, n+1)`.
fn size_bounds(m: &Matcher) -> Result<(Option<i64>, Option<i64>)> {
    match m {
        Matcher::Single(Value::Number(n)) => Ok((Some(*n as i64), Some(*n as i64 + 1))),
        Matcher::Range(lo, hi) => Ok((size_bound(lo.as_ref())?, size_bound(hi.as_ref())?)),
        _ => bail!("size expects a number"),
    }
}

fn size_bound(v: Option<&Value>) -> Result<Option<i64>> {
    match v {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(Some(*n as i64)),
        Some(_) => bail!("size bound must be a number"),
    }
}
