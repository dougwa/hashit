//! The scanner: query string → [`Query`].
//!
//! Hand-written, no parser-combinator dependency. Terms are separated by
//! top-level whitespace (whitespace inside quotes or `{…}` does not split). Each
//! term is an optional `+`/`-` [`Presence`], an optional `word:` [`Field`], then
//! a [`Value`] or an `A..B` [`Matcher::Range`].

use anyhow::{bail, Context, Result};
use chrono::FixedOffset;

use super::ast::{Field, Matcher, Op, Presence, Query, Term, Value};
use super::date::{CalUnit, DateExpr, Keyword, PartialDate, Unit};

/// Parse a query string into its AST.
pub fn parse(input: &str) -> Result<Query> {
    let mut s = Scanner::new(input);
    let mut terms = Vec::new();
    s.skip_ws();
    while !s.at_end() {
        terms.push(parse_term(&mut s)?);
        s.skip_ws();
    }
    Ok(Query {
        terms,
        limit: 0,
        offset: 0,
    })
}

fn parse_term(s: &mut Scanner) -> Result<Term> {
    let presence = match s.peek() {
        Some('+') => {
            s.bump();
            Presence::Must
        }
        Some('-') => {
            s.bump();
            Presence::MustNot
        }
        _ => Presence::Must,
    };

    // An optional `field` + operator — a bare identifier or a quoted string,
    // immediately followed by an operator (`:=`, `=`, or `:`; longest match first
    // so `:=` wins over `:`). With no operator, rewind: it all belongs to the value.
    let save = s.pos();
    let (field, op) = match parse_field_name(s)? {
        Some(f) => match parse_op(s) {
            Some(op) => (f, op),
            None => {
                s.set_pos(save);
                (Field::Default, Op::Match)
            }
        },
        None => {
            s.set_pos(save);
            (Field::Default, Op::Match)
        }
    };

    let matcher = parse_matcher(s, &field)?;
    Ok(Term {
        presence,
        field,
        op,
        matcher,
    })
}

/// Consume a match operator if one is next: `:=` → [`Op::Like`], `=` →
/// [`Op::Exact`], `:` → [`Op::Match`]. `:=` is checked first (longest match).
fn parse_op(s: &mut Scanner) -> Option<Op> {
    if s.eat_str(":=") {
        Some(Op::Like)
    } else if s.peek() == Some('=') {
        s.bump();
        Some(Op::Exact)
    } else if s.peek() == Some(':') {
        s.bump();
        Some(Op::Match)
    } else {
        None
    }
}

fn parse_matcher(s: &mut Scanner, field: &Field) -> Result<Matcher> {
    // Open lower bound: `..B`.
    if s.eat_str("..") {
        return Ok(Matcher::Range(None, Some(parse_single(s, field)?)));
    }
    let first = parse_single(s, field)?;
    if s.eat_str("..") {
        // Open upper bound `A..` when nothing follows the operator.
        if s.at_term_end() {
            return Ok(Matcher::Range(Some(first), None));
        }
        return Ok(Matcher::Range(Some(first), Some(parse_single(s, field)?)));
    }
    Ok(Matcher::Single(first))
}

fn parse_single(s: &mut Scanner, field: &Field) -> Result<Value> {
    match s.peek() {
        Some('{') => Ok(Value::Date(parse_date(&s.take_braced()?)?)),
        Some('\'') | Some('"') => Ok(Value::Text(s.take_quoted()?)),
        _ => {
            let w = s.take_word();
            if w.is_empty() {
                bail!("expected a value");
            }
            if matches!(field, Field::Size) {
                Ok(Value::Number(parse_size(&w)?))
            } else {
                Ok(Value::Text(w))
            }
        }
    }
}

/// Read a field name: a quoted string (taken literally) or a bare identifier.
/// In a bare identifier `-` is an alias for `:`, so a colon-bearing tag key like
/// `EXIF:Model` can be written `exif-model` without quoting; quote the name
/// (`"EXIF:Model"`) when you need the colons or a literal `-`.
fn parse_field_name(s: &mut Scanner) -> Result<Option<Field>> {
    match s.peek() {
        Some('\'') | Some('"') => Ok(Some(resolve_field(&s.take_quoted()?))),
        _ => {
            let ident = s.take_while(is_field_char);
            if ident.is_empty() {
                Ok(None)
            } else {
                Ok(Some(resolve_field(&ident.replace('-', ":"))))
            }
        }
    }
}

/// Map a field word to a [`Field`]. The known set is small and explicit; every
/// other name resolves to a metadata key. This is the table to extend when a new
/// column becomes searchable.
pub fn resolve_field(word: &str) -> Field {
    match word {
        "name" => Field::Name,
        "path" => Field::Path,
        "dir" => Field::Dir,
        "hash" => Field::Hash,
        "algo" => Field::Algo,
        "size" => Field::Size,
        "mtime" => Field::Mtime,
        "ext" => Field::Ext,
        "file_type" | "type" => Field::FileType,
        other => Field::Meta(other.to_string()),
    }
}

/// Parse a size value: digits with an optional binary suffix (`k`/`m`/`g` =
/// KiB/MiB/GiB).
fn parse_size(w: &str) -> Result<u64> {
    let (digits, mult) = match w.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&w[..w.len() - 1], 1024),
        Some(b'm') | Some(b'M') => (&w[..w.len() - 1], 1024 * 1024),
        Some(b'g') | Some(b'G') => (&w[..w.len() - 1], 1024 * 1024 * 1024),
        _ => (w, 1),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid size {w:?}"))?;
    Ok(n * mult)
}

// ---- date expressions ------------------------------------------------------

/// Parse the contents of a `{…}` date expression (braces already stripped).
///
/// Disambiguation on the first non-space byte: an ASCII digit → [`DateExpr::Absolute`]
/// (a year); `+`/`-` → [`DateExpr::Offset`]; a letter → [`DateExpr::Keyword`].
pub fn parse_date(body: &str) -> Result<DateExpr> {
    let t = body.trim();
    let first = t.bytes().next().context("empty date expression")?;
    if first.is_ascii_digit() {
        parse_absolute(t)
    } else if first == b'+' || first == b'-' {
        parse_offset(t)
    } else {
        parse_keyword(t)
    }
}

fn parse_absolute(s: &str) -> Result<DateExpr> {
    let (body, tz) = split_tz(s)?;
    let (date_part, time_part) = match body.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };

    let mut d = date_part.split('-');
    let year: i32 = d
        .next()
        .context("missing year")?
        .parse()
        .context("invalid year")?;
    let month = parse_opt(d.next(), "month")?;
    let day = parse_opt(d.next(), "day")?;

    let (mut hour, mut min, mut sec) = (None, None, None);
    if let Some(t) = time_part {
        let mut it = t.split(':');
        hour = parse_opt(it.next(), "hour")?;
        min = parse_opt(it.next(), "minute")?;
        sec = parse_opt(it.next(), "second")?;
    }

    Ok(DateExpr::Absolute(PartialDate {
        year,
        month,
        day,
        hour,
        min,
        sec,
        tz,
    }))
}

/// Strip a trailing `Z` or `±hh:mm` offset, returning the remaining body and the
/// offset (`None` = local time).
fn split_tz(s: &str) -> Result<(&str, Option<FixedOffset>)> {
    if let Some(rest) = s.strip_suffix('Z') {
        return Ok((rest, Some(FixedOffset::east_opt(0).unwrap())));
    }
    let b = s.as_bytes();
    if b.len() >= 6 {
        let tail = &b[b.len() - 6..];
        let signed = tail[0] == b'+' || tail[0] == b'-';
        if signed && tail[3] == b':' && tail[1..3].iter().all(u8::is_ascii_digit)
            && tail[4..6].iter().all(u8::is_ascii_digit)
        {
            let h: i32 = s[s.len() - 5..s.len() - 3].parse().unwrap();
            let m: i32 = s[s.len() - 2..].parse().unwrap();
            let sign = if tail[0] == b'+' { 1 } else { -1 };
            let off = FixedOffset::east_opt(sign * (h * 3600 + m * 60))
                .context("offset out of range")?;
            return Ok((&s[..s.len() - 6], Some(off)));
        }
    }
    Ok((s, None))
}

fn parse_offset(s: &str) -> Result<DateExpr> {
    let sign: i8 = if s.starts_with('-') { -1 } else { 1 };
    let rest = &s[1..];
    let split = rest
        .find(|c: char| !c.is_ascii_digit())
        .context("offset needs a unit")?;
    let n: i64 = rest[..split].parse().context("invalid offset magnitude")?;
    let unit = match &rest[split..] {
        "s" => Unit::Sec,
        "m" => Unit::Min,
        "h" => Unit::Hour,
        "d" => Unit::Day,
        "w" => Unit::Week,
        "mo" => Unit::Month,
        "y" => Unit::Year,
        other => bail!("unknown date unit {other:?}"),
    };
    Ok(DateExpr::Offset { sign, n, unit })
}

fn parse_keyword(s: &str) -> Result<DateExpr> {
    let lower = s.to_ascii_lowercase();
    let kw = match lower.as_str() {
        "now" => Keyword::Now,
        "today" => Keyword::Today,
        "yesterday" => Keyword::Yesterday,
        "tomorrow" => Keyword::Tomorrow,
        other => {
            let mut it = other.split_whitespace();
            let qualifier = it.next().unwrap_or_default();
            let unit = match it.next() {
                Some("day") => CalUnit::Day,
                Some("week") => CalUnit::Week,
                Some("month") => CalUnit::Month,
                Some("year") => CalUnit::Year,
                _ => bail!("unknown date keyword {s:?}"),
            };
            match qualifier {
                "this" => Keyword::This(unit),
                "last" => Keyword::Last(unit),
                "next" => Keyword::Next(unit),
                _ => bail!("unknown date keyword {s:?}"),
            }
        }
    };
    Ok(DateExpr::Keyword(kw))
}

fn parse_opt(s: Option<&str>, what: &str) -> Result<Option<u32>> {
    match s {
        None => Ok(None),
        Some(v) => Ok(Some(v.parse().with_context(|| format!("invalid {what}"))?)),
    }
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

// ---- the scanner ------------------------------------------------------------

struct Scanner {
    chars: Vec<char>,
    i: usize,
}

impl Scanner {
    fn new(s: &str) -> Self {
        Scanner {
            chars: s.chars().collect(),
            i: 0,
        }
    }

    fn at_end(&self) -> bool {
        self.i >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }

    fn pos(&self) -> usize {
        self.i
    }

    fn set_pos(&mut self, p: usize) {
        self.i = p;
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.i += 1;
        }
    }

    /// True at end-of-input or the next top-level whitespace (i.e. term boundary).
    fn at_term_end(&self) -> bool {
        matches!(self.peek(), None | Some(' ') | Some('\t') | Some('\n') | Some('\r'))
    }

    fn take_while(&mut self, pred: impl Fn(char) -> bool) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if pred(c) {
                out.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        out
    }

    /// Consume `lit` if it is next; report whether it matched.
    fn eat_str(&mut self, lit: &str) -> bool {
        let lc: Vec<char> = lit.chars().collect();
        if self.chars[self.i..].starts_with(&lc) {
            self.i += lc.len();
            true
        } else {
            false
        }
    }

    /// A bare word: up to the next whitespace or the `..` range operator.
    fn take_word(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '.' && self.chars.get(self.i + 1) == Some(&'.') {
                break;
            }
            out.push(c);
            self.i += 1;
        }
        out
    }

    /// The contents of a `{…}` group (assumes the next char is `{`).
    fn take_braced(&mut self) -> Result<String> {
        self.bump(); // consume '{'
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('}') => return Ok(out),
                Some(c) => out.push(c),
                None => bail!("unterminated '{{' in query"),
            }
        }
    }

    /// A single- or double-quoted string, unescaping `\\`, `\'`, and `\"`.
    fn take_quoted(&mut self) -> Result<String> {
        let quote = self.bump().expect("caller checked for a quote");
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('\\') => match self.bump() {
                    Some(c) => out.push(c),
                    None => bail!("trailing backslash in quoted string"),
                },
                Some(c) if c == quote => return Ok(out),
                Some(c) => out.push(c),
                None => bail!("unterminated quoted string"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(q: &str) -> Term {
        let mut parsed = parse(q).unwrap();
        assert_eq!(parsed.terms.len(), 1, "expected exactly one term in {q:?}");
        parsed.terms.pop().unwrap()
    }

    #[test]
    fn resolve_field_falls_through_to_meta() {
        assert_eq!(resolve_field("camera"), Field::Meta("camera".into()));
        assert_eq!(resolve_field("mtime"), Field::Mtime);
        assert_eq!(resolve_field("type"), Field::FileType);
    }

    #[test]
    fn bare_word_is_a_default_field_term() {
        let t = one("report");
        assert_eq!(t.presence, Presence::Must);
        assert_eq!(t.field, Field::Default);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("report".into())));
    }

    #[test]
    fn presence_and_field() {
        let t = one("-ext:tmp");
        assert_eq!(t.presence, Presence::MustNot);
        assert_eq!(t.field, Field::Ext);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("tmp".into())));
    }

    #[test]
    fn quoted_field_name_allows_colons() {
        let t = one(r#""EXIF:Model":Canon"#);
        assert_eq!(t.field, Field::Meta("EXIF:Model".into()));
        assert_eq!(t.matcher, Matcher::Single(Value::Text("Canon".into())));
    }

    #[test]
    fn dash_is_an_alias_for_colon_in_field_names() {
        let t = one("exif-model:Canon");
        assert_eq!(t.field, Field::Meta("exif:model".into()));
        assert_eq!(t.matcher, Matcher::Single(Value::Text("Canon".into())));
    }

    #[test]
    fn dashed_value_without_colon_stays_a_default_term() {
        let t = one("foo-bar");
        assert_eq!(t.field, Field::Default);
        assert_eq!(t.op, Op::Match);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("foo-bar".into())));
    }

    #[test]
    fn colon_is_the_default_match_operator() {
        let t = one("name:report");
        assert_eq!(t.field, Field::Name);
        assert_eq!(t.op, Op::Match);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("report".into())));
    }

    #[test]
    fn equals_is_the_exact_operator_and_dir_resolves() {
        let t = one("dir=/a/b/c");
        assert_eq!(t.field, Field::Dir);
        assert_eq!(t.op, Op::Exact);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("/a/b/c".into())));
    }

    #[test]
    fn colon_equals_is_the_like_operator_and_beats_colon() {
        let t = one("dir:=/a/b/%");
        assert_eq!(t.field, Field::Dir);
        assert_eq!(t.op, Op::Like);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("/a/b/%".into())));
    }

    #[test]
    fn equals_after_a_name_is_an_operator_like_colon() {
        // `a=b` mirrors `a:b`: `a` is a (meta) field, `=` the exact operator.
        // A literal `=` in a default-field search must be quoted ("a=b").
        let t = one("a=b");
        assert_eq!(t.field, Field::Meta("a".into()));
        assert_eq!(t.op, Op::Exact);
        assert_eq!(t.matcher, Matcher::Single(Value::Text("b".into())));

        let quoted = one(r#""a=b""#);
        assert_eq!(quoted.field, Field::Default);
        assert_eq!(quoted.matcher, Matcher::Single(Value::Text("a=b".into())));
    }

    #[test]
    fn negated_quoted_field() {
        let t = one(r#"-"EXIF:Model":Canon"#);
        assert_eq!(t.presence, Presence::MustNot);
        assert_eq!(t.field, Field::Meta("EXIF:Model".into()));
    }

    #[test]
    fn quoted_value_unescapes() {
        let t = one(r#"name:"a \"q\" name""#);
        assert_eq!(t.matcher, Matcher::Single(Value::Text(r#"a "q" name"#.into())));
    }

    #[test]
    fn size_suffix_and_open_range() {
        let t = one("size:1m..");
        assert_eq!(
            t.matcher,
            Matcher::Range(Some(Value::Number(1024 * 1024)), None)
        );
    }

    #[test]
    fn size_closed_range() {
        let t = one("size:1k..4k");
        assert_eq!(
            t.matcher,
            Matcher::Range(Some(Value::Number(1024)), Some(Value::Number(4096)))
        );
    }

    #[test]
    fn date_range_with_open_low_bound() {
        let t = one("mtime:..{2020}");
        match t.matcher {
            Matcher::Range(None, Some(Value::Date(DateExpr::Absolute(p)))) => {
                assert_eq!(p.year, 2020);
            }
            other => panic!("unexpected matcher: {other:?}"),
        }
    }

    #[test]
    fn multiple_terms_split_on_whitespace_respecting_quotes() {
        let q = parse(r#"photos +ext:jpg name:'two words' mtime:{last month}"#).unwrap();
        assert_eq!(q.terms.len(), 4);
        assert_eq!(
            q.terms[2].matcher,
            Matcher::Single(Value::Text("two words".into()))
        );
    }

    #[test]
    fn parse_date_forms() {
        assert_eq!(
            parse_date("2024-03-15").unwrap(),
            DateExpr::Absolute(PartialDate {
                year: 2024,
                month: Some(3),
                day: Some(15),
                hour: None,
                min: None,
                sec: None,
                tz: None,
            })
        );
        assert_eq!(
            parse_date("-7d").unwrap(),
            DateExpr::Offset {
                sign: -1,
                n: 7,
                unit: Unit::Day
            }
        );
        assert_eq!(
            parse_date("-3mo").unwrap(),
            DateExpr::Offset {
                sign: -1,
                n: 3,
                unit: Unit::Month
            }
        );
        assert_eq!(
            parse_date("last month").unwrap(),
            DateExpr::Keyword(Keyword::Last(CalUnit::Month))
        );
        assert_eq!(parse_date("today").unwrap(), DateExpr::Keyword(Keyword::Today));
    }

    #[test]
    fn parse_date_with_explicit_offset() {
        let d = parse_date("2024-03-15+09:00").unwrap();
        match d {
            DateExpr::Absolute(p) => {
                assert_eq!(p.day, Some(15));
                assert_eq!(p.tz, FixedOffset::east_opt(9 * 3600));
            }
            other => panic!("expected absolute, got {other:?}"),
        }
    }

    #[test]
    fn parse_date_utc_z() {
        let d = parse_date("2024-03-15Z").unwrap();
        match d {
            DateExpr::Absolute(p) => assert_eq!(p.tz, FixedOffset::east_opt(0)),
            other => panic!("expected absolute, got {other:?}"),
        }
    }
}
