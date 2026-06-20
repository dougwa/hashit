//! Date expressions and their resolution to half-open `[start, end)` intervals.
//!
//! Every date denotes an interval, not an instant; precision sets the width. The
//! subtle rule is that an [`DateExpr::Offset`] behaves differently as a *bare*
//! term than as a *range bound*: bare `{-7d}` is the window `now-7d .. now`, but
//! in `{-3mo}..{-1mo}` each side is the *point* `now ± n`. So resolution exposes
//! three entry points: [`DateExpr::resolve`] (bare term), [`DateExpr::start`]
//! (low range bound), and [`DateExpr::end`] (high range bound).

use chrono::{
    DateTime, Datelike, Days, Duration, FixedOffset, Local, Months, NaiveDate, NaiveDateTime,
    TimeZone,
};

/// A half-open interval `[start, end)` in nanoseconds since the Unix epoch.
/// `None` on a side means open (unbounded) there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl Interval {
    pub fn ns(start: i64, end: i64) -> Self {
        Interval {
            start: Some(start),
            end: Some(end),
        }
    }

    pub const UNBOUNDED: Interval = Interval {
        start: None,
        end: None,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DateExpr {
    /// A truncated ISO-8601 timestamp.
    Absolute(PartialDate),
    /// A signed duration from `now`; the sign is mandatory in the surface syntax.
    Offset { sign: i8, n: i64, unit: Unit },
    /// A calendar keyword (`today`, `last month`, …).
    Keyword(Keyword),
}

/// A possibly-truncated absolute date. Missing components default to their
/// minimum to form `start`; the least-significant *present* component sets the
/// interval width. `tz` of `None` means the machine's local time zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialDate {
    pub year: i32,
    pub month: Option<u32>,
    pub day: Option<u32>,
    pub hour: Option<u32>,
    pub min: Option<u32>,
    pub sec: Option<u32>,
    pub tz: Option<FixedOffset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Sec,
    Min,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    Now,
    Today,
    Yesterday,
    Tomorrow,
    This(CalUnit),
    Last(CalUnit),
    Next(CalUnit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalUnit {
    Day,
    Week,
    Month,
    Year,
}

impl DateExpr {
    /// Interval for a bare `field:{D}` term.
    pub fn resolve(&self, now: DateTime<Local>) -> Interval {
        match self {
            DateExpr::Absolute(p) => p.interval(),
            DateExpr::Keyword(k) => k.interval(now),
            DateExpr::Offset { .. } => {
                // A bare offset is the window between its point and now.
                let p = self.point(now);
                let n = to_ns(now);
                Interval::ns(p.min(n), p.max(n))
            }
        }
    }

    /// The value to use as the `start` when `D` is the low side of a range.
    pub fn start(&self, now: DateTime<Local>) -> Option<i64> {
        match self {
            DateExpr::Offset { .. } => Some(self.point(now)),
            _ => self.resolve(now).start,
        }
    }

    /// The value to use as the `end` when `D` is the high side of a range.
    pub fn end(&self, now: DateTime<Local>) -> Option<i64> {
        match self {
            DateExpr::Offset { .. } => Some(self.point(now)),
            _ => self.resolve(now).end,
        }
    }

    /// The single instant an offset denotes; for absolute/keyword forms, the
    /// start of their interval.
    fn point(&self, now: DateTime<Local>) -> i64 {
        match self {
            DateExpr::Offset { sign, n, unit } => to_ns(shift(now, *sign as i64 * *n, *unit)),
            DateExpr::Absolute(p) => p.interval().start.unwrap_or_else(|| to_ns(now)),
            DateExpr::Keyword(k) => k.interval(now).start.unwrap_or_else(|| to_ns(now)),
        }
    }
}

impl PartialDate {
    pub fn interval(&self) -> Interval {
        let start_naive = NaiveDate::from_ymd_opt(
            self.year,
            self.month.unwrap_or(1),
            self.day.unwrap_or(1),
        )
        .and_then(|d| {
            d.and_hms_opt(
                self.hour.unwrap_or(0),
                self.min.unwrap_or(0),
                self.sec.unwrap_or(0),
            )
        })
        .expect("valid partial date");

        let start = self.naive_to_ns(start_naive);
        let end = self.naive_to_ns(self.add_one_unit(start_naive));
        Interval::ns(start, end)
    }

    /// Advance by one unit at the precision of the least-significant present field.
    fn add_one_unit(&self, t: NaiveDateTime) -> NaiveDateTime {
        if self.sec.is_some() {
            t + Duration::seconds(1)
        } else if self.min.is_some() {
            t + Duration::minutes(1)
        } else if self.hour.is_some() {
            t + Duration::hours(1)
        } else if self.day.is_some() {
            t + Duration::days(1)
        } else if self.month.is_some() {
            t + Months::new(1)
        } else {
            t + Months::new(12) // year precision
        }
    }

    fn naive_to_ns(&self, naive: NaiveDateTime) -> i64 {
        match self.tz {
            // FixedOffset has no DST, so the mapping is always unambiguous.
            Some(off) => to_ns(off.from_local_datetime(&naive).single().expect("fixed offset")),
            None => to_ns(local_from_naive(naive)),
        }
    }
}

impl Keyword {
    pub fn interval(&self, now: DateTime<Local>) -> Interval {
        match self {
            Keyword::Now => {
                let n = to_ns(now);
                Interval::ns(n, n)
            }
            Keyword::Today => cal_interval(now, CalUnit::Day, 0),
            Keyword::Yesterday => cal_interval(now, CalUnit::Day, -1),
            Keyword::Tomorrow => cal_interval(now, CalUnit::Day, 1),
            Keyword::This(u) => cal_interval(now, *u, 0),
            Keyword::Last(u) => cal_interval(now, *u, -1),
            Keyword::Next(u) => cal_interval(now, *u, 1),
        }
    }
}

/// The calendar interval `offset` units (of `unit`) away from the one containing
/// `now`, snapped to local boundaries. ISO weeks (Monday start).
fn cal_interval(now: DateTime<Local>, unit: CalUnit, offset: i64) -> Interval {
    let today = now.date_naive();
    let (start_date, end_date) = match unit {
        CalUnit::Day => {
            let d = shift_days(today, offset);
            (d, shift_days(d, 1))
        }
        CalUnit::Week => {
            let monday = shift_days(today, -(today.weekday().num_days_from_monday() as i64));
            let s = shift_days(monday, offset * 7);
            (s, shift_days(s, 7))
        }
        CalUnit::Month => {
            let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
            let s = shift_months(first, offset);
            (s, shift_months(s, 1))
        }
        CalUnit::Year => {
            let first = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap();
            let s = shift_months(first, offset * 12);
            (s, shift_months(s, 12))
        }
    };
    let start = local_from_naive(start_date.and_hms_opt(0, 0, 0).unwrap());
    let end = local_from_naive(end_date.and_hms_opt(0, 0, 0).unwrap());
    Interval::ns(to_ns(start), to_ns(end))
}

/// Shift an instant by `n` units, calendar-correct for months and years.
fn shift(t: DateTime<Local>, n: i64, unit: Unit) -> DateTime<Local> {
    match unit {
        Unit::Sec => t + Duration::seconds(n),
        Unit::Min => t + Duration::minutes(n),
        Unit::Hour => t + Duration::hours(n),
        Unit::Day => t + Duration::days(n),
        Unit::Week => t + Duration::weeks(n),
        Unit::Month => add_months(t, n),
        Unit::Year => add_months(t, n * 12),
    }
}

fn add_months(t: DateTime<Local>, n: i64) -> DateTime<Local> {
    if n >= 0 {
        t + Months::new(n as u32)
    } else {
        t - Months::new((-n) as u32)
    }
}

fn shift_days(d: NaiveDate, n: i64) -> NaiveDate {
    if n >= 0 {
        d + Days::new(n as u64)
    } else {
        d - Days::new((-n) as u64)
    }
}

fn shift_months(d: NaiveDate, n: i64) -> NaiveDate {
    if n >= 0 {
        d + Months::new(n as u32)
    } else {
        d - Months::new((-n) as u32)
    }
}

/// Resolve a local wall-clock time to an instant, taking the earliest candidate
/// across a DST fold and falling back through a gap.
fn local_from_naive(naive: NaiveDateTime) -> DateTime<Local> {
    Local
        .from_local_datetime(&naive)
        .earliest()
        .unwrap_or_else(|| Local.from_utc_datetime(&naive))
}

fn to_ns<Tz: TimeZone>(dt: DateTime<Tz>) -> i64 {
    dt.timestamp_nanos_opt()
        .expect("timestamp within representable range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const DAY_NS: i64 = 24 * 3600 * 1_000_000_000;

    fn noon() -> DateTime<Local> {
        // Noon avoids DST edge cases in any sane zone.
        Local
            .with_ymd_and_hms(2026, 6, 19, 12, 0, 0)
            .single()
            .unwrap()
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn utc_ns(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap()
    }

    #[test]
    fn absolute_year_in_utc() {
        let p = PartialDate {
            year: 2024,
            month: None,
            day: None,
            hour: None,
            min: None,
            sec: None,
            tz: Some(utc()),
        };
        assert_eq!(
            p.interval(),
            Interval::ns(utc_ns(2024, 1, 1, 0, 0, 0), utc_ns(2025, 1, 1, 0, 0, 0))
        );
    }

    #[test]
    fn absolute_february_has_leap_day_width() {
        let feb = PartialDate {
            year: 2024, // leap year
            month: Some(2),
            day: None,
            hour: None,
            min: None,
            sec: None,
            tz: Some(utc()),
        };
        let iv = feb.interval();
        assert_eq!(iv.end.unwrap() - iv.start.unwrap(), 29 * DAY_NS);
    }

    #[test]
    fn absolute_day_respects_offset() {
        let off = FixedOffset::east_opt(9 * 3600).unwrap();
        let p = PartialDate {
            year: 2024,
            month: Some(3),
            day: Some(15),
            hour: None,
            min: None,
            sec: None,
            tz: Some(off),
        };
        let iv = p.interval();
        assert_eq!(iv.end.unwrap() - iv.start.unwrap(), DAY_NS);
        // Local midnight at +09:00 is 15:00Z the previous day.
        assert_eq!(iv.start, Some(utc_ns(2024, 3, 14, 15, 0, 0)));
    }

    #[test]
    fn offset_is_a_point_anchored_at_now() {
        let now = noon();
        let d = DateExpr::Offset {
            sign: -1,
            n: 7,
            unit: Unit::Day,
        };
        let expected = to_ns(now - Duration::days(7));
        assert_eq!(d.start(now), Some(expected));
        assert_eq!(d.end(now), Some(expected));
    }

    #[test]
    fn bare_offset_is_the_window_to_now() {
        let now = noon();
        let d = DateExpr::Offset {
            sign: -1,
            n: 1,
            unit: Unit::Day,
        };
        let iv = d.resolve(now);
        assert_eq!(iv.start, Some(to_ns(now - Duration::days(1))));
        assert_eq!(iv.end, Some(to_ns(now)));
    }

    #[test]
    fn rolling_day_differs_from_calendar_yesterday() {
        let now = noon();
        let rolling = DateExpr::Offset {
            sign: -1,
            n: 1,
            unit: Unit::Day,
        }
        .resolve(now);
        let calendar = DateExpr::Keyword(Keyword::Yesterday).resolve(now);
        assert_ne!(rolling.start, calendar.start);
    }

    #[test]
    fn today_contains_now_and_yesterday_abuts_it() {
        let now = noon();
        let today = Keyword::Today.interval(now);
        let yesterday = Keyword::Yesterday.interval(now);
        let n = to_ns(now);
        assert!(today.start.unwrap() <= n && n < today.end.unwrap());
        assert_eq!(yesterday.end, today.start);
    }

    #[test]
    fn offset_range_bound_is_a_point_before_now() {
        // In `{-3mo}..{-1mo}` the high bound must be now-1mo, not the window's end.
        let now = noon();
        let hi = DateExpr::Offset {
            sign: -1,
            n: 1,
            unit: Unit::Month,
        };
        assert!(hi.end(now).unwrap() < to_ns(now));
        // ...whereas the *bare* term's end snaps to now.
        assert_eq!(hi.resolve(now).end, Some(to_ns(now)));
    }
}
