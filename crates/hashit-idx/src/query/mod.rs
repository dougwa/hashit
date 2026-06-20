//! The query language: a flat string of AND-combined terms.
//!
//! Pipeline: `parse` (string → [`ast::Query`]) → `lower` (AST + a captured
//! `now` → [`lower::Filter`]) → the SQL builder in [`crate::store`] consumes the
//! `Filter`. Date resolution ([`date`]) turns each `{…}` into a half-open
//! `[start, end)` interval in nanoseconds since the Unix epoch — the unit of
//! `files.mtime_ns`.
//!
//! See `QUERY.md` at the repo root for the language spec.
// Scaffolding: not wired into the Search service yet, so the re-exports and
// helpers below look unused to the bin target.
#![allow(dead_code, unused_imports)]

pub mod ast;
pub mod date;
pub mod lower;
pub mod parse;

pub use ast::{Op, Query};
pub use date::{DateExpr, Interval};
pub use lower::{lower, Filter, NumCol, Pred, TextCol, TextMode};
pub use parse::parse;
