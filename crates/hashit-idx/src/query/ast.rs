//! The parsed query AST — the output of [`super::parse`], before any `now`-anchored
//! date resolution or lowering onto SQL columns.

use super::date::DateExpr;

/// A whole query string. Terms are AND-combined; a file matches when it
/// satisfies every term.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub presence: Presence,
    pub field: Field,
    pub matcher: Matcher,
}

/// `+` (default) requires a match; `-` requires the absence of one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    #[default]
    Must,
    MustNot,
}

/// Which field a term targets. Resolution is lenient: a name that isn't a known
/// column becomes [`Field::Meta`] (an extracted tag or a user property key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    /// No `field:` was given — match against the default field set.
    Default,
    Name,
    Path,
    Hash,
    Algo,
    Size,
    /// The date field (`files.mtime_ns`).
    Mtime,
    Ext,
    FileType,
    /// A tag or property key.
    Meta(String),
}

/// The right-hand side of a term, before resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Matcher {
    Single(Value),
    /// `A..B`, `A..`, or `..B`; `None` is an open side.
    Range(Option<Value>, Option<Value>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A word or a (already-unescaped) quoted string.
    Text(String),
    /// A number, e.g. a `size` with any `k`/`m`/`g` suffix already applied.
    Number(u64),
    Date(DateExpr),
}

impl Value {
    /// The text form, for lowering onto a metadata-key predicate.
    pub fn as_text(&self) -> String {
        match self {
            Value::Text(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Date(_) => String::new(),
        }
    }

    pub fn as_date(&self) -> Option<&DateExpr> {
        match self {
            Value::Date(d) => Some(d),
            _ => None,
        }
    }
}
