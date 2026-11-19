//! Cypher's temporal types: `DATE`, `LOCAL TIME`, `ZONED TIME`, `LOCAL DATETIME`,
//! `ZONED DATETIME` and `DURATION`.
//!
//! Dates use the proleptic Gregorian calendar and count days from 1970-01-01. Times
//! count nanoseconds from midnight. Zoned values carry a UTC offset in seconds; a zoned
//! date-time may also carry a named time zone (`Europe/Berlin`, see [`crate::tz`]), whose
//! rules then decide its offset. A zoned date-time keeps its local wall-clock fields and
//! its offset, and compares and equals by instant.
//!
//! A duration has three independent parts, as in Cypher: months, days and seconds (with
//! nanoseconds). Adding one to a date-time adds the months first (clamping the day to the
//! end of a shorter month), then the days, then the seconds.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use crate::tz::Tz;
use crate::{Error, Result, Value};

pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
pub const NANOS_PER_DAY: i64 = SECONDS_PER_DAY * NANOS_PER_SECOND;
/// Years outside `-MAX_YEAR..=MAX_YEAR` are rejected.
pub const MAX_YEAR: i64 = 999_999_999;
/// Average days per month (365.2425 / 12), for fractional months.
const DAYS_PER_MONTH: f64 = 30.436875;
/// Largest UTC offset, in seconds (±18:00).
const MAX_OFFSET: i32 = 18 * 3600;

/// A temporal value. Equality and hashing are structural (two zoned date-times at the
/// same instant with different offsets differ); Cypher equality is [`Temporal::cypher_eq`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Temporal {
    /// Days since 1970-01-01.
    Date(i64),
    /// Nanoseconds since midnight.
    LocalTime(i64),
    /// Nanoseconds since midnight, local to `offset` (seconds east of UTC).
    Time { nanos: i64, offset: i32 },
    LocalDateTime { days: i64, nanos: i64 },
    /// Local date and time at `offset`, in the named time zone `zone` if there is one
    /// (`offset` is then the zone's offset at that time).
    DateTime { days: i64, nanos: i64, offset: i32, zone: Option<Tz> },
    Duration(Duration),
}

/// A Cypher duration: months, days and seconds are kept apart, because their lengths in
/// seconds vary. `nanos` is in `0..1_000_000_000` (a negative fraction of a second is a
/// negative `seconds` plus a positive `nanos`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Duration {
    pub months: i64,
    pub days: i64,
    pub seconds: i64,
    pub nanos: i32,
}

/// The kinds of temporal values, for constructors and error messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemporalKind {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    DateTime,
    Duration,
}

impl TemporalKind {
    /// The Cypher function that constructs this kind (`date`, `localtime`, ...).
    pub fn function(self) -> &'static str {
        match self {
            TemporalKind::Date => "date",
            TemporalKind::LocalTime => "localtime",
            TemporalKind::Time => "time",
            TemporalKind::LocalDateTime => "localdatetime",
            TemporalKind::DateTime => "datetime",
            TemporalKind::Duration => "duration",
        }
    }
}

fn err(msg: impl Into<String>) -> Error {
    Error::Runtime(msg.into())
}

fn overflow() -> Error {
    err("temporal value out of range")
}

// ---- calendar -------------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `(year, month, day)` of a day number.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// ISO day of the week: Monday = 1 … Sunday = 7.
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

/// The Monday of ISO week 1 of `week_year`.
fn week_one(week_year: i64) -> i64 {
    let jan4 = days_from_civil(week_year, 1, 4);
    jan4 - (weekday(jan4) - 1)
}

/// `(week-based year, ISO week)` of a day.
fn iso_week(days: i64) -> (i64, i64) {
    let thursday = days - weekday(days) + 4;
    let (wy, _, _) = civil_from_days(thursday);
    (wy, (thursday - days_from_civil(wy, 1, 1)) / 7 + 1)
}

fn weeks_in_year(week_year: i64) -> i64 {
    (week_one(week_year + 1) - week_one(week_year)) / 7
}

fn check_year(y: i64) -> Result<()> {
    if (-MAX_YEAR..=MAX_YEAR).contains(&y) {
        Ok(())
    } else {
        Err(err(format!("year {y} is out of range")))
    }
}

/// A checked calendar date.
fn ymd(y: i64, m: i64, d: i64) -> Result<i64> {
    check_year(y)?;
    if !(1..=12).contains(&m) {
        return Err(err(format!("invalid month {m}")));
    }
    let max = days_in_month(y, m as u32) as i64;
    if !(1..=max).contains(&d) {
        return Err(err(format!("invalid day {d} for {y}-{m:02}")));
    }
    Ok(days_from_civil(y, m as u32, d as u32))
}

fn week_date(y: i64, w: i64, dow: i64) -> Result<i64> {
    check_year(y)?;
    if !(1..=weeks_in_year(y)).contains(&w) {
        return Err(err(format!("invalid week {w} of {y}")));
    }
    if !(1..=7).contains(&dow) {
        return Err(err(format!("invalid day of week {dow}")));
    }
    Ok(week_one(y) + (w - 1) * 7 + dow - 1)
}

fn ordinal_date(y: i64, doy: i64) -> Result<i64> {
    check_year(y)?;
    let max = if is_leap(y) { 366 } else { 365 };
    if !(1..=max).contains(&doy) {
        return Err(err(format!("invalid day of year {doy}")));
    }
    Ok(days_from_civil(y, 1, 1) + doy - 1)
}

fn quarter_date(y: i64, q: i64, doq: i64) -> Result<i64> {
    check_year(y)?;
    if !(1..=4).contains(&q) {
        return Err(err(format!("invalid quarter {q}")));
    }
    let start = days_from_civil(y, (q as u32 - 1) * 3 + 1, 1);
    let end = if q == 4 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, q as u32 * 3 + 1, 1) };
    if !(1..=end - start).contains(&doq) {
        return Err(err(format!("invalid day of quarter {doq}")));
    }
    Ok(start + doq - 1)
}

fn check_days(days: i64) -> Result<i64> {
    check_year(civil_from_days(days).0)?;
    Ok(days)
}

/// Adds `months` to a date, clamping the day to the length of the target month.
fn add_months(days: i64, months: i64) -> Result<i64> {
    if months == 0 {
        return Ok(days);
    }
    let (y, m, d) = civil_from_days(days);
    let total = (y.checked_mul(12).ok_or_else(overflow)?)
        .checked_add(m as i64 - 1 + months)
        .ok_or_else(overflow)?;
    let (ny, nm) = (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32);
    check_year(ny)?;
    Ok(days_from_civil(ny, nm, d.min(days_in_month(ny, nm))))
}

fn hms(h: i64, m: i64, s: i64, nanos: i64) -> Result<i64> {
    if !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..60).contains(&s) {
        return Err(err(format!("invalid time {h:02}:{m:02}:{s:02}")));
    }
    if !(0..NANOS_PER_SECOND).contains(&nanos) {
        return Err(err(format!("invalid fraction of a second ({nanos} ns)")));
    }
    Ok(((h * 60 + m) * 60 + s) * NANOS_PER_SECOND + nanos)
}

fn check_offset(o: i64) -> Result<i32> {
    if o.abs() <= MAX_OFFSET as i64 {
        Ok(o as i32)
    } else {
        Err(err(format!("UTC offset of {o} seconds is out of range")))
    }
}

/// The current date and time in UTC: `(days, nanos of day)`.
pub fn now_utc() -> (i64, i64) {
    let d = crate::clock::unix_time().unwrap_or_default();
    let total = d.as_nanos() as i128;
    (
        total.div_euclid(NANOS_PER_DAY as i128) as i64,
        total.rem_euclid(NANOS_PER_DAY as i128) as i64,
    )
}

// ---- parsing --------------------------------------------------------------------------

/// A cursor over an ASCII temporal string.
struct Cursor<'a> {
    s: &'a [u8],
    pos: usize,
    text: &'a str,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Cursor {
            s: text.as_bytes(),
            pos: 0,
            text,
        }
    }

    fn bad(&self, what: &str) -> Error {
        err(format!("invalid {what} `{}`", self.text))
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn done(&self) -> bool {
        self.pos == self.s.len()
    }

    /// Up to `max` digits (at least `min`); `None` if fewer than `min`.
    fn digits(&mut self, min: usize, max: usize) -> Option<(i64, usize)> {
        let start = self.pos;
        let mut v: i64 = 0;
        while self.pos - start < max {
            match self.peek() {
                Some(c @ b'0'..=b'9') => {
                    v = v * 10 + (c - b'0') as i64;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let n = self.pos - start;
        if n < min {
            self.pos = start;
            None
        } else {
            Some((v, n))
        }
    }

    /// Digits until a non-digit, counted.
    fn count_digits(&self) -> usize {
        self.s[self.pos..].iter().take_while(|c| c.is_ascii_digit()).count()
    }
}

/// `YYYY-MM-DD`, `YYYYMMDD`, `YYYY-MM`, `YYYY`, `YYYY-Www[-D]`, `YYYYWww[D]`,
/// `YYYY-DDD`, `YYYYDDD`; years beyond four digits need a sign (`+10000-01-01`).
fn parse_date_part(c: &mut Cursor) -> Result<i64> {
    let sign = if c.eat(b'-') {
        -1
    } else {
        c.eat(b'+');
        1
    };
    let signed = c.pos > 0;
    let (y, _) = if signed { c.digits(4, 9) } else { c.digits(4, 4) }.ok_or_else(|| c.bad("date"))?;
    let y = sign * y;
    let dashed = c.eat(b'-');
    if c.eat(b'W') {
        let (w, _) = c.digits(2, 2).ok_or_else(|| c.bad("date"))?;
        let dow = if (dashed && c.eat(b'-')) || (!dashed && c.peek().is_some_and(|b| b.is_ascii_digit())) {
            c.digits(1, 1).ok_or_else(|| c.bad("date"))?.0
        } else {
            1
        };
        return week_date(y, w, dow);
    }
    let n = c.count_digits();
    if dashed {
        match n {
            3 => ordinal_date(y, c.digits(3, 3).unwrap().0),
            2 => {
                let m = c.digits(2, 2).unwrap().0;
                let d = if c.eat(b'-') { c.digits(2, 2).ok_or_else(|| c.bad("date"))?.0 } else { 1 };
                ymd(y, m, d)
            }
            _ => Err(c.bad("date")),
        }
    } else {
        match n {
            0 => ymd(y, 1, 1),
            2 => ymd(y, c.digits(2, 2).unwrap().0, 1),
            3 => ordinal_date(y, c.digits(3, 3).unwrap().0),
            4 => {
                let m = c.digits(2, 2).unwrap().0;
                ymd(y, m, c.digits(2, 2).unwrap().0)
            }
            _ => Err(c.bad("date")),
        }
    }
}

/// `HH[:MM[:SS[.fffffffff]]]` or `HH[MM[SS[.fffffffff]]]`.
fn parse_time_part(c: &mut Cursor) -> Result<i64> {
    let (h, _) = c.digits(2, 2).ok_or_else(|| c.bad("time"))?;
    let colon = c.eat(b':');
    let mut m = 0;
    let mut s = 0;
    let mut nanos = 0;
    if let Some((v, _)) = c.digits(2, 2) {
        m = v;
        if (!colon || c.eat(b':')) && let Some((v, _)) = c.digits(2, 2) {
            s = v;
            if c.eat(b'.') || c.eat(b',') {
                let (f, n) = c.digits(1, 9).ok_or_else(|| c.bad("time"))?;
                nanos = f * 10i64.pow(9 - n as u32);
            }
        }
    } else if colon {
        return Err(c.bad("time"));
    }
    hms(h, m, s, nanos)
}

/// `Z`, `±HH`, `±HHMM`, `±HH:MM` or `±HH:MM:SS`, if present, then a named zone
/// (`[Europe/Paris]`).
fn parse_offset_part(c: &mut Cursor) -> Result<(Option<i32>, Option<Tz>)> {
    let offset = if c.eat(b'Z') || c.eat(b'z') {
        Some(0)
    } else if matches!(c.peek(), Some(b'+' | b'-')) {
        let sign = if c.eat(b'-') { -1 } else { c.pos += 1; 1 };
        let (h, _) = c.digits(2, 2).ok_or_else(|| c.bad("UTC offset"))?;
        let colon = c.eat(b':');
        let m = match c.digits(2, 2) {
            Some((m, _)) => m,
            None if colon => return Err(c.bad("UTC offset")),
            None => 0,
        };
        let s = if (colon && c.eat(b':')) || (!colon && c.count_digits() == 2) {
            c.digits(2, 2).ok_or_else(|| c.bad("UTC offset"))?.0
        } else {
            0
        };
        if h > 18 || m > 59 || s > 59 {
            return Err(c.bad("UTC offset"));
        }
        Some(check_offset(sign * (h * 3600 + m * 60 + s))?)
    } else {
        None
    };
    let zone = if c.eat(b'[') {
        let rest = &c.text[c.pos..];
        let end = rest.find(']').ok_or_else(|| c.bad("time zone"))?;
        let name = &rest[..end];
        c.pos += end + 1;
        Some(find_zone(name)?)
    } else {
        None
    };
    Ok((offset, zone))
}

/// `zone`'s offset now (a time of day in a named zone takes today's offset).
fn zone_offset_now(zone: Tz) -> i32 {
    let (days, nanos) = now_utc();
    zone.offset_at(days * SECONDS_PER_DAY + nanos / NANOS_PER_SECOND)
}

/// The named time zone `name`.
fn find_zone(name: &str) -> Result<Tz> {
    Tz::find(name).ok_or_else(|| err(format!("unknown time zone `{name}`")))
}

/// A `timezone` field: a UTC offset or a named zone.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ZoneSpec {
    Offset(i32),
    Named(Tz),
}

fn parse_zone(text: &str) -> Result<ZoneSpec> {
    match parse_offset(text) {
        Ok(o) => Ok(ZoneSpec::Offset(o)),
        Err(_) if !text.starts_with(['+', '-']) => find_zone(text).map(ZoneSpec::Named),
        Err(e) => Err(e),
    }
}

/// A date-time at the local `days` / `nanos` in `zone`. A local time in a gap moves on
/// by the gap; in an overlap, `prefer` picks the offset if it is one of the two.
fn zoned(days: i64, nanos: i64, zone: Tz, prefer: Option<i32>) -> Result<Temporal> {
    let secs = days * SECONDS_PER_DAY + nanos.div_euclid(NANOS_PER_SECOND);
    let (moved, offset) = zone.resolve(secs, prefer);
    let local = days as i128 * NANOS_PER_DAY as i128 + nanos as i128 + (moved - secs) as i128 * NANOS_PER_SECOND as i128;
    Ok(Temporal::DateTime {
        days: check_days(local.div_euclid(NANOS_PER_DAY as i128) as i64)?,
        nanos: local.rem_euclid(NANOS_PER_DAY as i128) as i64,
        offset,
        zone: Some(zone),
    })
}

/// The date-time at the instant `instant` (nanoseconds since the epoch) in `zone`, or at
/// the fixed `offset` when there is no zone.
fn at_instant(instant: i128, offset: i32, zone: Option<Tz>) -> Result<Temporal> {
    let offset = match zone {
        Some(z) => z.offset_at(instant.div_euclid(NANOS_PER_SECOND as i128) as i64),
        None => offset,
    };
    let local = instant + offset as i128 * NANOS_PER_SECOND as i128;
    Ok(Temporal::DateTime {
        days: check_days(i64::try_from(local.div_euclid(NANOS_PER_DAY as i128)).map_err(|_| overflow())?)?,
        nanos: local.rem_euclid(NANOS_PER_DAY as i128) as i64,
        offset,
        zone,
    })
}

/// Parses a UTC offset given on its own (`timezone` fields): `Z`, `UTC`, `+01:00`, ….
pub fn parse_offset(text: &str) -> Result<i32> {
    if text.eq_ignore_ascii_case("utc") {
        return Ok(0);
    }
    let mut c = Cursor::new(text);
    match parse_offset_part(&mut c)? {
        (Some(o), None) if c.done() => Ok(o),
        _ => Err(Error::Unsupported(format!(
            "time zone `{text}` is not supported; use a UTC offset such as +01:00"
        ))),
    }
}

impl Temporal {
    /// Parses the ISO 8601 text form of a value of `kind`.
    pub fn parse(kind: TemporalKind, text: &str) -> Result<Temporal> {
        let t = text.trim();
        let mut c = Cursor::new(t);
        let what = kind.function();
        let v = match kind {
            TemporalKind::Duration => return Duration::parse(t).map(Temporal::Duration),
            TemporalKind::Date => Temporal::Date(parse_date_part(&mut c)?),
            TemporalKind::LocalTime | TemporalKind::Time => {
                c.eat(b'T');
                let nanos = parse_time_part(&mut c)?;
                let (offset, zone) = parse_offset_part(&mut c)?;
                match (kind, offset, zone) {
                    (TemporalKind::LocalTime, None, None) => Temporal::LocalTime(nanos),
                    (TemporalKind::LocalTime, ..) => return Err(c.bad("local time (it has an offset)")),
                    // A time keeps only an offset; a zone alone gives today's.
                    (_, o, z) => Temporal::Time {
                        nanos,
                        offset: o.or_else(|| z.map(zone_offset_now)).unwrap_or(0),
                    },
                }
            }
            TemporalKind::LocalDateTime | TemporalKind::DateTime => {
                let days = parse_date_part(&mut c)?;
                let nanos = if c.eat(b'T') || c.eat(b't') { parse_time_part(&mut c)? } else { 0 };
                let (offset, zone) = parse_offset_part(&mut c)?;
                match (kind, offset, zone) {
                    (TemporalKind::LocalDateTime, None, None) => Temporal::LocalDateTime { days, nanos },
                    (TemporalKind::LocalDateTime, ..) => {
                        return Err(c.bad("local datetime (it has an offset)"));
                    }
                    (_, o, Some(z)) => zoned(days, nanos, z, o)?,
                    (_, o, None) => Temporal::DateTime {
                        days,
                        nanos,
                        offset: o.unwrap_or(0),
                        zone: None,
                    },
                }
            }
        };
        if !c.done() {
            return Err(err(format!("invalid {what} `{t}`")));
        }
        Ok(v)
    }

    /// The Cypher type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Temporal::Date(_) => "DATE",
            Temporal::LocalTime(_) => "LOCAL TIME",
            Temporal::Time { .. } => "ZONED TIME",
            Temporal::LocalDateTime { .. } => "LOCAL DATETIME",
            Temporal::DateTime { .. } => "ZONED DATETIME",
            Temporal::Duration(_) => "DURATION",
        }
    }

    pub fn kind(&self) -> TemporalKind {
        match self {
            Temporal::Date(_) => TemporalKind::Date,
            Temporal::LocalTime(_) => TemporalKind::LocalTime,
            Temporal::Time { .. } => TemporalKind::Time,
            Temporal::LocalDateTime { .. } => TemporalKind::LocalDateTime,
            Temporal::DateTime { .. } => TemporalKind::DateTime,
            Temporal::Duration(_) => TemporalKind::Duration,
        }
    }

    /// The date part (days since the epoch), if the value has one.
    pub fn date_days(&self) -> Option<i64> {
        match *self {
            Temporal::Date(d) | Temporal::LocalDateTime { days: d, .. } | Temporal::DateTime { days: d, .. } => Some(d),
            _ => None,
        }
    }

    /// The local time of day (nanoseconds), if the value has one.
    pub fn time_nanos(&self) -> Option<i64> {
        match *self {
            Temporal::LocalTime(n)
            | Temporal::Time { nanos: n, .. }
            | Temporal::LocalDateTime { nanos: n, .. }
            | Temporal::DateTime { nanos: n, .. } => Some(n),
            _ => None,
        }
    }

    /// A zoned date-time at the local `days` / `nanos` in `zone` (see [`Tz::resolve`]
    /// for local times that occur twice or not at all; `prefer` picks the offset then).
    pub fn in_zone(days: i64, nanos: i64, zone: Tz, prefer: Option<i32>) -> Result<Temporal> {
        zoned(days, nanos, zone, prefer)
    }

    /// The zoned date-time at `instant` (nanoseconds since the epoch) in `zone`.
    pub fn at_instant_in(instant: i128, zone: Tz) -> Result<Temporal> {
        at_instant(instant, 0, Some(zone))
    }

    /// The named time zone of a zoned date-time, if it has one.
    pub fn zone(&self) -> Option<Tz> {
        match *self {
            Temporal::DateTime { zone, .. } => zone,
            _ => None,
        }
    }

    /// The UTC offset in seconds, if the value is zoned.
    pub fn offset(&self) -> Option<i32> {
        match *self {
            Temporal::Time { offset, .. } | Temporal::DateTime { offset, .. } => Some(offset),
            _ => None,
        }
    }

    /// Nanoseconds since the epoch of a zoned date-time.
    fn instant(days: i64, nanos: i64, offset: i32) -> i128 {
        days as i128 * NANOS_PER_DAY as i128 + nanos as i128 - offset as i128 * NANOS_PER_SECOND as i128
    }

    /// A zoned time's time of day in UTC (wrapping around midnight).
    fn utc_time(nanos: i64, offset: i32) -> i64 {
        (nanos - offset as i64 * NANOS_PER_SECOND).rem_euclid(NANOS_PER_DAY)
    }

    /// The same value in a normal form where Cypher-equal values are structurally equal:
    /// zoned values move to UTC. Used for index keys.
    pub fn canonical(&self) -> Temporal {
        match *self {
            Temporal::Time { nanos, offset } => Temporal::Time {
                nanos: Self::utc_time(nanos, offset),
                offset: 0,
            },
            Temporal::DateTime { days, nanos, offset, .. } => {
                let i = Self::instant(days, nanos, offset);
                Temporal::DateTime {
                    days: i.div_euclid(NANOS_PER_DAY as i128) as i64,
                    nanos: i.rem_euclid(NANOS_PER_DAY as i128) as i64,
                    offset: 0,
                    zone: None,
                }
            }
            other => other,
        }
    }

    /// Cypher `=` between temporals: same kind, and the same instant for zoned values.
    pub fn cypher_eq(&self, other: &Temporal) -> bool {
        self.kind() == other.kind() && self.canonical() == other.canonical()
    }

    /// Cypher `<` and friends: values of the same kind compare (zoned values by instant);
    /// durations do not compare.
    pub fn cypher_cmp(&self, other: &Temporal) -> Option<Ordering> {
        Some(match (*self, *other) {
            (Temporal::Date(a), Temporal::Date(b)) => a.cmp(&b),
            (Temporal::LocalTime(a), Temporal::LocalTime(b)) => a.cmp(&b),
            (Temporal::Time { nanos: a, offset: oa }, Temporal::Time { nanos: b, offset: ob }) => {
                Self::utc_time(a, oa).cmp(&Self::utc_time(b, ob))
            }
            (Temporal::LocalDateTime { days: a, nanos: na }, Temporal::LocalDateTime { days: b, nanos: nb }) => {
                (a, na).cmp(&(b, nb))
            }
            (
                Temporal::DateTime { days: a, nanos: na, offset: oa, .. },
                Temporal::DateTime { days: b, nanos: nb, offset: ob, .. },
            ) => Self::instant(a, na, oa).cmp(&Self::instant(b, nb, ob)),
            _ => return None,
        })
    }

    /// Rank among temporal kinds in Cypher's global sort order.
    pub fn order_rank(&self) -> u8 {
        match self {
            Temporal::DateTime { .. } => 0,
            Temporal::LocalDateTime { .. } => 1,
            Temporal::Date(_) => 2,
            Temporal::Time { .. } => 3,
            Temporal::LocalTime(_) => 4,
            Temporal::Duration(_) => 5,
        }
    }

    /// Total order for `ORDER BY`: by kind, then by value. Durations order by their
    /// approximate length, then by their parts.
    pub fn order_cmp(&self, other: &Temporal) -> Ordering {
        self.order_rank()
            .cmp(&other.order_rank())
            .then_with(|| match (self, other) {
                (Temporal::Duration(a), Temporal::Duration(b)) => a
                    .approx_nanos()
                    .cmp(&b.approx_nanos())
                    .then_with(|| (a.months, a.days, a.seconds, a.nanos).cmp(&(b.months, b.days, b.seconds, b.nanos))),
                _ => self
                    .cypher_cmp(other)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| self.offset().cmp(&other.offset())),
            })
    }

    // ---- construction -----------------------------------------------------------------

    /// `date(x)`, `localtime(x)`, …: from a string, a map of fields, or another temporal
    /// value. `now` is the current UTC `(days, nanos)`, used when there is no argument.
    pub fn construct(kind: TemporalKind, arg: Option<&Value>, now: (i64, i64)) -> Result<Value> {
        let what = kind.function();
        // `{timezone: …}` alone: the current time there.
        if let (Some(Value::Map(m)), true) = (arg, kind != TemporalKind::Duration)
            && m.len() == 1
            && let Some(z) = m.get("timezone")
        {
            return Self::now_in(kind, Some(z), now);
        }
        let t = match arg {
            None => Self::convert(kind, &Temporal::DateTime { days: now.0, nanos: now.1, offset: 0, zone: None })?,
            Some(Value::Null) => return Ok(Value::Null),
            Some(Value::String(s)) => Self::parse(kind, s)?,
            Some(Value::Temporal(t)) => Self::convert(kind, t)?,
            Some(Value::Map(m)) => match kind {
                TemporalKind::Duration => Temporal::Duration(Duration::from_map(m)?),
                _ => Self::from_map(kind, m)?,
            },
            Some(other) => {
                return Err(Error::Type(format!("{what}() does not accept {}", other.type_name())));
            }
        };
        Ok(Value::Temporal(t))
    }

    /// `date.statement(zone)` and friends: the current time `now` (UTC days and nanos) as
    /// a `kind` value in `zone` (a UTC offset or a named zone; UTC when there is none).
    /// A `null` zone gives `null`.
    pub fn now_in(kind: TemporalKind, zone: Option<&Value>, now: (i64, i64)) -> Result<Value> {
        let instant = now.0 as i128 * NANOS_PER_DAY as i128 + now.1 as i128;
        let dt = match zone {
            None => at_instant(instant, 0, None)?,
            Some(Value::Null) => return Ok(Value::Null),
            Some(Value::String(z)) => match parse_zone(z)? {
                ZoneSpec::Offset(o) => at_instant(instant, o, None)?,
                ZoneSpec::Named(tz) => at_instant(instant, 0, Some(tz))?,
            },
            Some(v) => {
                return Err(Error::Type(format!("{}(): a time zone must be a string, got {}", kind.function(), v.type_name())));
            }
        };
        Ok(Value::Temporal(Self::convert(kind, &dt)?))
    }

    /// `kind`'s value made from another temporal value.
    fn convert(kind: TemporalKind, t: &Temporal) -> Result<Temporal> {
        let bad = || {
            Error::Type(format!("cannot make a {} from a {}", kind.function(), t.type_name()))
        };
        let date = t.date_days();
        let time = t.time_nanos();
        Ok(match kind {
            TemporalKind::Date => Temporal::Date(date.ok_or_else(bad)?),
            TemporalKind::LocalTime => Temporal::LocalTime(time.ok_or_else(bad)?),
            TemporalKind::Time => Temporal::Time {
                nanos: time.ok_or_else(bad)?,
                offset: t.offset().unwrap_or(0),
            },
            TemporalKind::LocalDateTime => Temporal::LocalDateTime {
                days: date.ok_or_else(bad)?,
                nanos: time.unwrap_or(0),
            },
            TemporalKind::DateTime => Temporal::DateTime {
                days: date.ok_or_else(bad)?,
                nanos: time.unwrap_or(0),
                offset: t.offset().unwrap_or(0),
                zone: t.zone(),
            },
            TemporalKind::Duration => match t {
                Temporal::Duration(_) => *t,
                _ => return Err(bad()),
            },
        })
    }

    /// From a map of fields: `{year, month, day}` (or `week` / `dayOfWeek`,
    /// `ordinalDay`, `quarter` / `dayOfQuarter`), `{hour, minute, second, millisecond,
    /// microsecond, nanosecond}`, `timezone`, and `date` / `time` / `datetime` to take
    /// fields from another value (explicit fields override them).
    fn from_map(kind: TemporalKind, m: &BTreeMap<String, Value>) -> Result<Temporal> {
        let what = kind.function();
        let int = |k: &str| -> Result<Option<i64>> {
            match m.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Int(i)) => Ok(Some(*i)),
                Some(Value::Float(f)) if f.fract() == 0.0 => Ok(Some(*f as i64)),
                Some(v) => Err(Error::Type(format!("{what}(): `{k}` must be an integer, got {}", v.type_name()))),
            }
        };
        let temporal = |k: &str| -> Result<Option<Temporal>> {
            match m.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Temporal(t)) => Ok(Some(*t)),
                Some(v) => Err(Error::Type(format!("{what}(): `{k}` must be a temporal value, got {}", v.type_name()))),
            }
        };
        const KNOWN: [&str; 19] = [
            "date", "time", "datetime", "year", "month", "day", "week", "dayOfWeek", "ordinalDay", "quarter",
            "dayOfQuarter", "hour", "minute", "second", "millisecond", "microsecond", "nanosecond", "timezone",
            "epochSeconds",
        ];
        if let Some(k) = m.keys().find(|k| !KNOWN.contains(&k.as_str())) {
            return Err(err(format!("{what}(): unknown field `{k}`")));
        }
        let (mut date, mut time, mut offset) = (None, None, None);
        // The named zone of a zoned date-time the fields start from.
        let mut base_zone = None;
        if let Some(t) = temporal("datetime")? {
            date = t.date_days();
            time = t.time_nanos();
            offset = t.offset();
            base_zone = t.zone();
        }
        if let Some(t) = temporal("date")? {
            date = Some(t.date_days().ok_or_else(|| err(format!("{what}(): `date` has no date")))?);
        }
        if let Some(t) = temporal("time")? {
            time = Some(t.time_nanos().ok_or_else(|| err(format!("{what}(): `time` has no time")))?);
            if t.offset().is_some() {
                offset = t.offset();
                base_zone = t.zone();
            }
        }
        // Date fields.
        let year = int("year")?;
        let (month, day) = (int("month")?, int("day")?);
        let (week, dow) = (int("week")?, int("dayOfWeek")?);
        let (ordinal, quarter, doq) = (int("ordinalDay")?, int("quarter")?, int("dayOfQuarter")?);
        let any_date = [year, month, day, week, dow, ordinal, quarter, doq].iter().any(Option::is_some);
        if any_date {
            let base = date.map(civil_from_days);
            let week_based = week.is_some() || (dow.is_some() && month.is_none() && day.is_none());
            // A week date counts from the base's week-based year (1816-12-30 is in 1817's week 1).
            let base_year = if week_based { date.map(|d| iso_week(d).0) } else { base.map(|b| b.0) };
            let y = year.or(base_year).ok_or_else(|| err(format!("{what}(): `year` is required")))?;
            date = Some(if week_based {
                let (bw, bd) = match date {
                    Some(d) => (iso_week(d).1, weekday(d)),
                    None => (1, 1),
                };
                week_date(y, week.unwrap_or(bw), dow.unwrap_or(bd))?
            } else if let Some(o) = ordinal {
                ordinal_date(y, o)?
            } else if let Some(q) = quarter {
                // Without `dayOfQuarter`, the base's day of its quarter.
                let base_doq = base.zip(date).map(|((by, bm, _), d)| {
                    d - days_from_civil(by, (bm - 1) / 3 * 3 + 1, 1) + 1
                });
                quarter_date(y, q, doq.or(base_doq).unwrap_or(1))?
            } else {
                ymd(
                    y,
                    month.or(base.map(|b| b.1 as i64)).unwrap_or(1),
                    day.or(base.map(|b| b.2 as i64)).unwrap_or(1),
                )?
            });
        }
        // Time fields.
        let fields = ["hour", "minute", "second", "millisecond", "microsecond", "nanosecond"];
        let given: Vec<Option<i64>> = fields.iter().map(|k| int(k)).collect::<Result<_>>()?;
        if given.iter().any(Option::is_some) {
            let base = time.unwrap_or(0);
            let (bh, bm, bs) = (
                base / 3_600_000_000_000,
                base / 60_000_000_000 % 60,
                base / NANOS_PER_SECOND % 60,
            );
            let h = given[0].unwrap_or(bh);
            let mi = given[1].unwrap_or(if given[0].is_some() { 0 } else { bm });
            let s = given[2].unwrap_or(if given[0].is_some() || given[1].is_some() { 0 } else { bs });
            let sub = if given[3..].iter().any(Option::is_some) {
                let (ms, us, ns) = (given[3].unwrap_or(0), given[4].unwrap_or(0), given[5].unwrap_or(0));
                if !(0..1000).contains(&ms) || !(0..1_000_000).contains(&us) || !(0..NANOS_PER_SECOND).contains(&ns) {
                    return Err(err(format!("{what}(): fraction of a second out of range")));
                }
                ms * 1_000_000 + us * 1000 + ns
            } else if given[0].is_some() || given[1].is_some() {
                0
            } else {
                // `second` alone keeps the base's fraction of a second.
                base % NANOS_PER_SECOND
            };
            time = Some(hms(h, mi, s, sub)?);
        }
        let zone = match m.get("timezone") {
            None | Some(Value::Null) => None,
            Some(Value::String(z)) => Some(parse_zone(z)?),
            Some(v) => return Err(Error::Type(format!("{what}(): `timezone` must be a string, got {}", v.type_name()))),
        };
        if let Some(e) = int("epochSeconds")? {
            let i = e as i128 * NANOS_PER_SECOND as i128;
            date = Some(i.div_euclid(NANOS_PER_DAY as i128) as i64);
            time = Some(i.rem_euclid(NANOS_PER_DAY as i128) as i64);
            offset = Some(0);
            base_zone = None;
        }
        let need_date = || date.ok_or_else(|| err(format!("{what}(): a date is required (`year`, or `date`)")));
        let need_time = || time.ok_or_else(|| err(format!("{what}(): a time is required (`hour`, or `time`)")));
        Ok(match kind {
            TemporalKind::Date => Temporal::Date(check_days(need_date()?)?),
            TemporalKind::LocalTime => Temporal::LocalTime(need_time()?),
            TemporalKind::Time => {
                let nanos = need_time()?;
                let to = match zone {
                    None => offset.unwrap_or(0),
                    Some(ZoneSpec::Offset(o)) => o,
                    Some(ZoneSpec::Named(z)) => match date {
                        Some(d) => z.resolve(d * SECONDS_PER_DAY + nanos / NANOS_PER_SECOND, offset).1,
                        None => zone_offset_now(z),
                    },
                };
                // A time with an offset moved to another one keeps its instant.
                let nanos = match offset {
                    Some(from) if from != to => (nanos + (to - from) as i64 * NANOS_PER_SECOND).rem_euclid(NANOS_PER_DAY),
                    _ => nanos,
                };
                Temporal::Time { nanos, offset: to }
            }
            TemporalKind::LocalDateTime => Temporal::LocalDateTime {
                days: check_days(need_date()?)?,
                nanos: time.unwrap_or(0),
            },
            TemporalKind::DateTime => {
                let (days, nanos) = (check_days(need_date()?)?, time.unwrap_or(0));
                // The fields read in the zone (or at the offset) they came from.
                let base = match (base_zone, offset) {
                    (Some(z), o) => Some(zoned(days, nanos, z, o)?),
                    (None, Some(o)) => Some(Temporal::DateTime { days, nanos, offset: o, zone: None }),
                    (None, None) => None,
                };
                match (base, zone) {
                    (Some(b), None) => b,
                    // A zoned value moved to another zone keeps its instant.
                    (Some(Temporal::DateTime { days, nanos, offset, .. }), Some(to)) => {
                        let i = Self::instant(days, nanos, offset);
                        match to {
                            ZoneSpec::Offset(o) => at_instant(i, o, None)?,
                            ZoneSpec::Named(z) => at_instant(i, 0, Some(z))?,
                        }
                    }
                    (_, Some(ZoneSpec::Named(z))) => zoned(days, nanos, z, None)?,
                    (_, Some(ZoneSpec::Offset(o))) => Temporal::DateTime { days, nanos, offset: o, zone: None },
                    (_, None) => Temporal::DateTime { days, nanos, offset: 0, zone: None },
                }
            }
            TemporalKind::Duration => unreachable!("durations are built by Duration::from_map"),
        })
    }

    /// `date.truncate(unit, t, fields)`, `datetime.truncate(…)`, …: `t` as a `kind`
    /// value, cut down to the start of its `unit` (`millennium`, `century`, `decade`,
    /// `year`, `weekYear`, `quarter`, `month`, `week`, `day`, `hour`, `minute`, `second`,
    /// `millisecond`, `microsecond`); then `fields` are set as in `kind`'s constructor.
    pub fn truncate(kind: TemporalKind, unit: &str, t: &Temporal, fields: &BTreeMap<String, Value>) -> Result<Temporal> {
        let what = format!("{}.truncate", kind.function());
        // The unit's rank: date units truncate the date, then 0 is `day`, and time units
        // count up to `microsecond`.
        const UNITS: [&str; 14] = [
            "millennium", "century", "decade", "year", "weekyear", "quarter", "month", "week", "day", "hour",
            "minute", "second", "millisecond", "microsecond",
        ];
        let lower = unit.to_ascii_lowercase();
        let rank = UNITS
            .iter()
            .position(|u| *u == lower)
            .ok_or_else(|| err(format!("{what}(): unknown unit `{unit}`")))?;
        let has_date = matches!(kind, TemporalKind::Date | TemporalKind::LocalDateTime | TemporalKind::DateTime);
        let has_time = !matches!(kind, TemporalKind::Date);
        if kind == TemporalKind::Duration || (!has_time && rank > 8) || (!has_date && rank < 8) {
            return Err(err(format!("{what}(): cannot truncate to `{unit}`")));
        }
        let bad = || Error::Type(format!("{what}() cannot truncate a {}", t.type_name()));
        let days = match has_date {
            true => {
                let d = t.date_days().ok_or_else(bad)?;
                let (y, m, _) = civil_from_days(d);
                Some(match rank {
                    0..=2 => {
                        let step = [1000, 100, 10][rank];
                        days_from_civil(y.div_euclid(step) * step, 1, 1)
                    }
                    3 => days_from_civil(y, 1, 1),
                    4 => week_one(iso_week(d).0),
                    5 => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1),
                    6 => days_from_civil(y, m, 1),
                    7 => d - (weekday(d) - 1),
                    _ => d,
                })
            }
            false => None,
        };
        let nanos = match has_time {
            true => {
                let n = t.time_nanos().or(days.map(|_| 0)).ok_or_else(bad)?;
                let step = match rank {
                    0..=8 => NANOS_PER_DAY,
                    9 => 3600 * NANOS_PER_SECOND,
                    10 => 60 * NANOS_PER_SECOND,
                    11 => NANOS_PER_SECOND,
                    12 => 1_000_000,
                    _ => 1000,
                };
                Some(n - n % step)
            }
            false => None,
        };
        let offset = t.offset().unwrap_or(0);
        let base = match kind {
            TemporalKind::Date => Temporal::Date(days.unwrap()),
            TemporalKind::LocalTime => Temporal::LocalTime(nanos.unwrap()),
            TemporalKind::Time => Temporal::Time { nanos: nanos.unwrap(), offset },
            TemporalKind::LocalDateTime => Temporal::LocalDateTime { days: days.unwrap(), nanos: nanos.unwrap() },
            _ => match t.zone() {
                Some(z) => zoned(days.unwrap(), nanos.unwrap(), z, Some(offset))?,
                None => Temporal::DateTime { days: days.unwrap(), nanos: nanos.unwrap(), offset, zone: None },
            },
        };
        if fields.is_empty() {
            return Ok(base);
        }
        // The truncated value as constructor fields, with `fields` over them. A
        // `nanosecond` adds to the milli- and microseconds that are left.
        let mut m = BTreeMap::new();
        if let Some(d) = days {
            if fields.contains_key("week") || fields.contains_key("dayOfWeek") {
                let (wy, w) = iso_week(d);
                m.insert("year".to_string(), Value::Int(wy));
                m.insert("week".to_string(), Value::Int(w));
                m.insert("dayOfWeek".to_string(), Value::Int(weekday(d)));
            } else {
                let (y, mo, dd) = civil_from_days(d);
                m.insert("year".to_string(), Value::Int(y));
                m.insert("month".to_string(), Value::Int(mo as i64));
                m.insert("day".to_string(), Value::Int(dd as i64));
            }
        }
        if let Some(n) = nanos {
            let s = n / NANOS_PER_SECOND;
            let sub = n % NANOS_PER_SECOND;
            for (k, v) in [
                ("hour", s / 3600),
                ("minute", s / 60 % 60),
                ("second", s % 60),
                ("millisecond", sub / 1_000_000),
                ("microsecond", sub / 1000 % 1000),
            ] {
                m.insert(k.to_string(), Value::Int(v));
            }
        }
        if matches!(kind, TemporalKind::Time | TemporalKind::DateTime) {
            let zone = match (kind, t.zone()) {
                (TemporalKind::DateTime, Some(z)) => z.name().to_string(),
                _ => offset_text(offset),
            };
            m.insert("timezone".to_string(), Value::String(zone));
        }
        for (k, v) in fields {
            m.insert(k.clone(), v.clone());
        }
        Self::from_map(kind, &m)
    }

    /// `datetime.fromepoch(seconds, nanos)`.
    pub fn from_epoch_nanos(nanos: i128) -> Result<Temporal> {
        let days = nanos.div_euclid(NANOS_PER_DAY as i128);
        let days = i64::try_from(days).map_err(|_| overflow())?;
        Ok(Temporal::DateTime {
            days: check_days(days)?,
            nanos: nanos.rem_euclid(NANOS_PER_DAY as i128) as i64,
            offset: 0,
            zone: None,
        })
    }

    // ---- fields ---------------------------------------------------------------------

    /// The field `key` (`year`, `hour`, `offset`, `days`, …); `None` if the value has no
    /// such field.
    pub fn field(&self, key: &str) -> Option<Value> {
        if let Temporal::Duration(d) = self {
            return d.field(key);
        }
        let int = |v: i64| Some(Value::Int(v));
        if let Some(days) = self.date_days() {
            let (y, m, d) = civil_from_days(days);
            match key {
                "year" => return int(y),
                "quarter" => return int((m as i64 - 1) / 3 + 1),
                "month" => return int(m as i64),
                "week" => return int(iso_week(days).1),
                "weekYear" => return int(iso_week(days).0),
                "day" => return int(d as i64),
                "ordinalDay" => return int(days - days_from_civil(y, 1, 1) + 1),
                "dayOfQuarter" => return int(days - days_from_civil(y, (m - 1) / 3 * 3 + 1, 1) + 1),
                "dayOfWeek" | "weekDay" => return int(weekday(days)),
                _ => {}
            }
        }
        if let Some(n) = self.time_nanos() {
            match key {
                "hour" => return int(n / 3_600_000_000_000),
                "minute" => return int(n / 60_000_000_000 % 60),
                "second" => return int(n / NANOS_PER_SECOND % 60),
                "millisecond" => return int(n % NANOS_PER_SECOND / 1_000_000),
                "microsecond" => return int(n % NANOS_PER_SECOND / 1000),
                "nanosecond" => return int(n % NANOS_PER_SECOND),
                _ => {}
            }
        }
        if let Some(o) = self.offset() {
            match key {
                "timezone" => {
                    return Some(Value::String(match self.zone() {
                        Some(z) => z.name().to_string(),
                        None => offset_text(o),
                    }));
                }
                "offset" => return Some(Value::String(offset_text(o))),
                "offsetMinutes" => return int(o as i64 / 60),
                "offsetSeconds" => return int(o as i64),
                _ => {}
            }
        }
        if let Temporal::DateTime { days, nanos, offset, .. } = *self {
            let i = Self::instant(days, nanos, offset);
            match key {
                "epochSeconds" => return int(i.div_euclid(NANOS_PER_SECOND as i128) as i64),
                "epochMillis" => return int(i.div_euclid(1_000_000) as i64),
                _ => {}
            }
        }
        None
    }

    // ---- arithmetic -----------------------------------------------------------------

    /// `self + d` for a date or time value.
    pub fn plus(&self, d: &Duration) -> Result<Temporal> {
        let total = d.seconds as i128 * NANOS_PER_SECOND as i128 + d.nanos as i128;
        let wrap = |n: i64| (n as i128 + total).rem_euclid(NANOS_PER_DAY as i128) as i64;
        let date_time = |days: i64, nanos: i64| -> Result<(i64, i64)> {
            let days = add_months(days, d.months)?.checked_add(d.days).ok_or_else(overflow)?;
            let t = nanos as i128 + total;
            let days = days
                .checked_add(i64::try_from(t.div_euclid(NANOS_PER_DAY as i128)).map_err(|_| overflow())?)
                .ok_or_else(overflow)?;
            Ok((check_days(days)?, t.rem_euclid(NANOS_PER_DAY as i128) as i64))
        };
        Ok(match *self {
            Temporal::Date(days) => {
                // Seconds count only in whole days.
                let extra = i64::try_from(total / NANOS_PER_DAY as i128).map_err(|_| overflow())?;
                let days = add_months(days, d.months)?
                    .checked_add(d.days)
                    .and_then(|x| x.checked_add(extra))
                    .ok_or_else(overflow)?;
                Temporal::Date(check_days(days)?)
            }
            Temporal::LocalTime(n) => Temporal::LocalTime(wrap(n)),
            Temporal::Time { nanos, offset } => Temporal::Time { nanos: wrap(nanos), offset },
            Temporal::LocalDateTime { days, nanos } => {
                let (days, nanos) = date_time(days, nanos)?;
                Temporal::LocalDateTime { days, nanos }
            }
            Temporal::DateTime { days, nanos, offset, zone: None } => {
                let (days, nanos) = date_time(days, nanos)?;
                Temporal::DateTime { days, nanos, offset, zone: None }
            }
            // In a named zone, months and days move the wall clock (keeping the offset
            // where it still fits); seconds move the instant.
            Temporal::DateTime { days, nanos, offset, zone: Some(z) } => {
                let days = add_months(days, d.months)?.checked_add(d.days).ok_or_else(overflow)?;
                let Temporal::DateTime { days, nanos, offset, .. } = zoned(check_days(days)?, nanos, z, Some(offset))? else {
                    unreachable!("zoned() makes a date-time")
                };
                at_instant(Self::instant(days, nanos, offset) + total, offset, Some(z))?
            }
            Temporal::Duration(a) => Temporal::Duration(a.add(d)?),
        })
    }

    /// `duration.between(a, b)` and its variants (`inMonths`, `inDays`, `inSeconds`):
    /// the duration from `a` to `b`. A value without a date only counts time; one without
    /// a time counts as midnight; zoned values compare at `a`'s offset.
    pub fn between(a: &Temporal, b: &Temporal, unit: BetweenUnit) -> Result<Duration> {
        if matches!(a, Temporal::Duration(_)) || matches!(b, Temporal::Duration(_)) {
            return Err(Error::Type("duration.between() needs two dates or times, not durations".into()));
        }
        if let (Some(z), BetweenUnit::Seconds) = (a.zone().or(b.zone()), unit) {
            // A local value counts in the zone, on the other value's date if it has none.
            let instant = |t: &Temporal, other: &Temporal| -> i128 {
                let days = t.date_days().or(other.date_days()).unwrap_or(0);
                let nanos = t.time_nanos().unwrap_or(0);
                let offset = t.offset().unwrap_or_else(|| z.resolve(days * SECONDS_PER_DAY + nanos / NANOS_PER_SECOND, None).1);
                Self::instant(days, nanos, offset)
            };
            return Duration::from_nanos(0, 0, instant(b, a) - instant(a, b));
        }
        // b's local fields at a's offset.
        let shift = match (a.offset(), b.offset()) {
            (Some(oa), Some(ob)) => (oa - ob) as i64 * NANOS_PER_SECOND,
            _ => 0,
        };
        let dated = a.date_days().is_some() && b.date_days().is_some();
        let pos = |t: &Temporal, shift: i64| -> i128 {
            let days = if dated { t.date_days().unwrap_or(0) } else { 0 };
            days as i128 * NANOS_PER_DAY as i128 + t.time_nanos().unwrap_or(0) as i128 + shift as i128
        };
        let (pa, pb) = (pos(a, 0), pos(b, shift));
        let split = |p: i128| (p.div_euclid(NANOS_PER_DAY as i128) as i64, p.rem_euclid(NANOS_PER_DAY as i128) as i64);
        let months = |pa: i128, pb: i128| -> i64 {
            if !dated {
                return 0;
            }
            let ((da, ta), (db, tb)) = (split(pa), split(pb));
            let ((ya, ma, dda), (yb, mb, ddb)) = (civil_from_days(da), civil_from_days(db));
            let mut m = (yb * 12 + mb as i64) - (ya * 12 + ma as i64);
            if m > 0 && (ddb, tb) < (dda, ta) {
                m -= 1;
            } else if m < 0 && (ddb, tb) > (dda, ta) {
                m += 1;
            }
            m
        };
        let days = |pa: i128, pb: i128| -> i64 { if dated { ((pb - pa) / NANOS_PER_DAY as i128) as i64 } else { 0 } };
        let secs = |pa: i128, pb: i128| Duration::from_nanos(0, 0, pb - pa);
        Ok(match unit {
            BetweenUnit::Months => Duration { months: months(pa, pb), ..Duration::default() },
            BetweenUnit::Days => Duration { days: days(pa, pb), ..Duration::default() },
            BetweenUnit::Seconds => secs(pa, pb)?,
            BetweenUnit::All => {
                let m = months(pa, pb);
                let pa = if m == 0 {
                    pa
                } else {
                    let (da, ta) = split(pa);
                    add_months(da, m)? as i128 * NANOS_PER_DAY as i128 + ta as i128
                };
                let d = days(pa, pb);
                let pa = pa + d as i128 * NANOS_PER_DAY as i128;
                let rest = secs(pa, pb)?;
                Duration { months: m, days: d, ..rest }
            }
        })
    }
}

/// What `duration.between` measures in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BetweenUnit {
    All,
    Months,
    Days,
    Seconds,
}

fn offset_text(o: i32) -> String {
    if o == 0 {
        return "Z".into();
    }
    let sign = if o < 0 { '-' } else { '+' };
    let a = o.unsigned_abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    if s == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    }
}

fn write_date(f: &mut fmt::Formatter<'_>, days: i64) -> fmt::Result {
    let (y, m, d) = civil_from_days(days);
    if (0..=9999).contains(&y) {
        write!(f, "{y:04}-{m:02}-{d:02}")
    } else if y < 0 {
        write!(f, "-{:04}-{m:02}-{d:02}", -y)
    } else {
        write!(f, "+{y}-{m:02}-{d:02}")
    }
}

/// A fraction of a second, without trailing zero groups: `.5` → `.500`, `.123456`.
fn write_fraction(f: &mut fmt::Formatter<'_>, nanos: i64) -> fmt::Result {
    if nanos == 0 {
        return Ok(());
    }
    if nanos % 1_000_000 == 0 {
        write!(f, ".{:03}", nanos / 1_000_000)
    } else if nanos % 1000 == 0 {
        write!(f, ".{:06}", nanos / 1000)
    } else {
        write!(f, ".{nanos:09}")
    }
}

/// `HH:MM`, plus `:SS` and a fraction when they are not zero (as Java and Neo4j).
fn write_time(f: &mut fmt::Formatter<'_>, n: i64) -> fmt::Result {
    let s = n / NANOS_PER_SECOND;
    write!(f, "{:02}:{:02}", s / 3600, s / 60 % 60)?;
    if n % (60 * NANOS_PER_SECOND) == 0 {
        return Ok(());
    }
    write!(f, ":{:02}", s % 60)?;
    write_fraction(f, n % NANOS_PER_SECOND)
}

impl fmt::Display for Temporal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Temporal::Date(d) => write_date(f, d),
            Temporal::LocalTime(n) => write_time(f, n),
            Temporal::Time { nanos, offset } => {
                write_time(f, nanos)?;
                f.write_str(&offset_text(offset))
            }
            Temporal::LocalDateTime { days, nanos } => {
                write_date(f, days)?;
                f.write_str("T")?;
                write_time(f, nanos)
            }
            Temporal::DateTime { days, nanos, offset, zone } => {
                write_date(f, days)?;
                f.write_str("T")?;
                write_time(f, nanos)?;
                f.write_str(&offset_text(offset))?;
                match zone {
                    Some(z) => write!(f, "[{}]", z.name()),
                    None => Ok(()),
                }
            }
            Temporal::Duration(d) => write!(f, "{d}"),
        }
    }
}

// ---- durations ------------------------------------------------------------------------

impl Duration {
    /// A duration of `months` and `days` plus `nanos` nanoseconds.
    pub fn from_nanos(months: i64, days: i64, nanos: i128) -> Result<Duration> {
        let seconds = i64::try_from(nanos.div_euclid(NANOS_PER_SECOND as i128)).map_err(|_| overflow())?;
        Ok(Duration {
            months,
            days,
            seconds,
            nanos: nanos.rem_euclid(NANOS_PER_SECOND as i128) as i32,
        })
    }

    fn total_nanos(&self) -> i128 {
        self.seconds as i128 * NANOS_PER_SECOND as i128 + self.nanos as i128
    }

    /// Approximate length, for ordering: months of average length, days of 24 hours.
    fn approx_nanos(&self) -> i128 {
        let month = (DAYS_PER_MONTH * SECONDS_PER_DAY as f64) as i128 * NANOS_PER_SECOND as i128;
        self.months as i128 * month + self.days as i128 * NANOS_PER_DAY as i128 + self.total_nanos()
    }

    pub fn add(&self, o: &Duration) -> Result<Duration> {
        Duration::from_nanos(
            self.months.checked_add(o.months).ok_or_else(overflow)?,
            self.days.checked_add(o.days).ok_or_else(overflow)?,
            self.total_nanos() + o.total_nanos(),
        )
    }

    pub fn negate(&self) -> Result<Duration> {
        Duration::from_nanos(
            self.months.checked_neg().ok_or_else(overflow)?,
            self.days.checked_neg().ok_or_else(overflow)?,
            -self.total_nanos(),
        )
    }

    /// `self * k`: whole multiples exactly; fractions of months spill into days (at the
    /// average month length) and fractions of days into seconds.
    pub fn scale(&self, k: f64) -> Result<Duration> {
        if !k.is_finite() {
            return Err(err("cannot multiply a duration by a non-finite number"));
        }
        if k.fract() == 0.0 && k.abs() < 9.0e15 {
            let k = k as i64;
            return Duration::from_nanos(
                self.months.checked_mul(k).ok_or_else(overflow)?,
                self.days.checked_mul(k).ok_or_else(overflow)?,
                self.total_nanos().checked_mul(k as i128).ok_or_else(overflow)?,
            );
        }
        self.scale_exact(k).unwrap_or_else(|| {
            Duration::cascade(self.months as f64 * k, self.days as f64 * k, self.total_nanos() as f64 * k)
        })
    }

    /// `self * k` computed exactly with `k` as the fraction `p / 2^n` it is, rounding
    /// only the final nanoseconds (half to even). `None` if it does not fit in `i128`.
    fn scale_exact(&self, k: f64) -> Option<Result<Duration>> {
        let (mut p, mut shift) = (k, 0u32);
        while p.fract() != 0.0 {
            p *= 2.0;
            shift += 1;
        }
        if p.abs() >= 9.0e15 || shift > 64 {
            return None;
        }
        let (p, q) = (p as i128, 1i128 << shift);
        const NANOS_PER_MONTH: i128 = 2_629_746 * NANOS_PER_SECOND as i128;
        let npd = NANOS_PER_DAY as i128;
        // Months: whole ones, the rest (in 1/q months) spills into days at the average length.
        let mq = (self.months as i128).checked_mul(p)?;
        let (m, rm) = (mq / q, mq % q);
        let x = (self.days as i128).checked_mul(p)?.checked_mul(npd)?.checked_add(rm.checked_mul(NANOS_PER_MONTH)?)?;
        let dq = q.checked_mul(npd)?;
        let (d, rd) = (x / dq, x % dq);
        let nq = rd.checked_add(self.total_nanos().checked_mul(p)?)?;
        let (mut n, r) = (nq.div_euclid(q), nq.rem_euclid(q));
        if 2 * r > q || (2 * r == q && n % 2 != 0) {
            n += 1;
        }
        Some(Duration::from_nanos(i64::try_from(m).ok()?, i64::try_from(d).ok()?, n))
    }

    /// Whole months, days and nanoseconds from fractional amounts.
    fn cascade(months: f64, days: f64, nanos: f64) -> Result<Duration> {
        let m = months.trunc();
        let days = days + (months - m) * DAYS_PER_MONTH;
        let d = days.trunc();
        let nanos = nanos + (days - d) * NANOS_PER_DAY as f64;
        let limit = i64::MAX as f64;
        if m.abs() >= limit || d.abs() >= limit || nanos.abs() >= 1.0e37 {
            return Err(overflow());
        }
        Duration::from_nanos(m as i64, d as i64, nanos.round() as i128)
    }

    /// `duration({years: 1, months: 2, days: 3, hours: 4, …})`; numbers may be
    /// fractional.
    pub fn from_map(m: &BTreeMap<String, Value>) -> Result<Duration> {
        let (mut months, mut days, mut nanos) = (0f64, 0f64, 0f64);
        let (mut im, mut id, mut inn): (i64, i64, i128) = (0, 0, 0);
        for (k, v) in m {
            let (unit_months, unit_days, unit_nanos): (i64, i64, i128) = match k.as_str() {
                "years" => (12, 0, 0),
                "quarters" => (3, 0, 0),
                "months" => (1, 0, 0),
                "weeks" => (0, 7, 0),
                "days" => (0, 1, 0),
                "hours" => (0, 0, 3_600_000_000_000),
                "minutes" => (0, 0, 60_000_000_000),
                "seconds" => (0, 0, NANOS_PER_SECOND as i128),
                "milliseconds" => (0, 0, 1_000_000),
                "microseconds" => (0, 0, 1000),
                "nanoseconds" => (0, 0, 1),
                _ => return Err(err(format!("duration(): unknown field `{k}`"))),
            };
            match v {
                Value::Null => {}
                Value::Int(i) => {
                    im = im.checked_add(i.checked_mul(unit_months).ok_or_else(overflow)?).ok_or_else(overflow)?;
                    id = id.checked_add(i.checked_mul(unit_days).ok_or_else(overflow)?).ok_or_else(overflow)?;
                    inn += *i as i128 * unit_nanos;
                }
                Value::Float(f) => {
                    months += f * unit_months as f64;
                    days += f * unit_days as f64;
                    nanos += f * unit_nanos as f64;
                }
                other => {
                    return Err(Error::Type(format!(
                        "duration(): `{k}` must be a number, got {}",
                        other.type_name()
                    )));
                }
            }
        }
        let frac = Duration::cascade(months, days, nanos)?;
        Duration::from_nanos(im, id, inn)?.add(&frac)
    }

    /// The ISO 8601 alternative form after the `P`: `YYYY-MM-DDThh:mm:ss[.fff]`. `None`
    /// if `rest` does not have that shape.
    fn parse_alternative(rest: &str) -> Option<Result<Duration>> {
        let b = rest.as_bytes();
        let shape = b"dddd-dd-ddTdd:dd:dd";
        if b.len() < shape.len()
            || !shape.iter().zip(b).all(|(p, c)| if *p == b'd' { c.is_ascii_digit() } else { p.eq_ignore_ascii_case(c) })
        {
            return None;
        }
        let num = |r: std::ops::Range<usize>| rest[r].parse::<i64>().unwrap();
        let mut nanos = (num(11..13) * 3600 + num(14..16) * 60 + num(17..19)) as i128 * NANOS_PER_SECOND as i128;
        match &rest[19..] {
            "" => {}
            f if f.len() >= 2 && f.len() <= 10 && f.starts_with(['.', ',']) && f[1..].bytes().all(|c| c.is_ascii_digit()) => {
                nanos += f[1..].parse::<i128>().unwrap() * 10i128.pow(10 - f.len() as u32);
            }
            _ => return None,
        }
        Some(Duration::from_nanos(num(0..4) * 12 + num(5..7), num(8..10), nanos))
    }

    /// ISO 8601: `P[nY][nM][nW][nD][T[nH][nM][nS]]`, each number optionally signed and
    /// with an optional fraction, or `PYYYY-MM-DDThh:mm:ss[.fff]`.
    pub fn parse(text: &str) -> Result<Duration> {
        let bad = || err(format!("invalid duration `{text}`"));
        let s = text.as_bytes();
        let mut i = 0;
        let mut neg_all = false;
        if s.first() == Some(&b'-') {
            neg_all = true;
            i += 1;
        }
        if !matches!(s.get(i), Some(b'P' | b'p')) {
            return Err(bad());
        }
        i += 1;
        if let Some(d) = Self::parse_alternative(&text[i..]) {
            return if neg_all { d?.negate() } else { d };
        }
        let (mut months, mut days, mut nanos): (i64, i64, i128) = (0, 0, 0);
        let mut in_time = false;
        let mut any = false;
        while i < s.len() {
            if matches!(s[i], b'T' | b't') && !in_time {
                in_time = true;
                i += 1;
                continue;
            }
            let neg = match s[i] {
                b'-' => {
                    i += 1;
                    true
                }
                b'+' => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let start = i;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return Err(bad());
            }
            let whole: i64 = text[start..i].parse().map_err(|_| bad())?;
            let (mut frac, mut digits) = (0i128, 0u32);
            if i < s.len() && matches!(s[i], b'.' | b',') {
                i += 1;
                let fs = i;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                if i == fs || i - fs > 9 {
                    return Err(bad());
                }
                frac = text[fs..i].parse().map_err(|_| bad())?;
                digits = (i - fs) as u32;
            }
            let unit = *s.get(i).ok_or_else(bad)?;
            i += 1;
            let sign: i128 = if neg { -1 } else { 1 };
            let scaled = |unit_nanos: i128| sign * (whole as i128 * unit_nanos + frac * unit_nanos / 10i128.pow(digits));
            // A fraction of a larger unit goes to whole days and the rest to time; a month
            // is 1/12 of the average Gregorian year (30.436875 days).
            const NANOS_PER_MONTH: i128 = 2_629_746 * NANOS_PER_SECOND as i128;
            let spill = |f: i128, days: &mut i64, nanos: &mut i128| {
                *days += (f / NANOS_PER_DAY as i128) as i64;
                *nanos += f % NANOS_PER_DAY as i128;
            };
            match (in_time, unit.to_ascii_uppercase()) {
                (false, b'Y') => {
                    months += sign as i64 * whole.checked_mul(12).ok_or_else(overflow)?;
                    // Twelfths of the fraction are months, the rest a fraction of a month.
                    let twelfths = frac * 12;
                    let scale = 10i128.pow(digits);
                    months += (sign * (twelfths / scale)) as i64;
                    spill(sign * (twelfths % scale) * NANOS_PER_MONTH / scale, &mut days, &mut nanos);
                }
                (false, b'M') => {
                    months += sign as i64 * whole;
                    spill(sign * frac * NANOS_PER_MONTH / 10i128.pow(digits), &mut days, &mut nanos);
                }
                (false, b'W') => {
                    days += sign as i64 * whole.checked_mul(7).ok_or_else(overflow)?;
                    spill(sign * frac * 7 * NANOS_PER_DAY as i128 / 10i128.pow(digits), &mut days, &mut nanos);
                }
                (false, b'D') => {
                    days += sign as i64 * whole;
                    nanos += sign * frac * NANOS_PER_DAY as i128 / 10i128.pow(digits);
                }
                (true, b'H') => nanos += scaled(3_600_000_000_000),
                (true, b'M') => nanos += scaled(60_000_000_000),
                (true, b'S') => nanos += scaled(NANOS_PER_SECOND as i128),
                _ => return Err(bad()),
            }
            any = true;
        }
        if !any {
            return Err(bad());
        }
        let d = Duration::from_nanos(months, days, nanos)?;
        if neg_all { d.negate() } else { Ok(d) }
    }

    /// A duration field: totals per unit within its group (`months`, `days`, `seconds`
    /// …), or the remainder within the next larger unit (`monthsOfYear`, …).
    pub fn field(&self, key: &str) -> Option<Value> {
        // Whole seconds rounded down, with a non-negative sub-second part (as Neo4j).
        let (ts, tn) = (self.seconds, self.nanos as i64);
        let v = match key {
            "years" => self.months / 12,
            "quarters" => self.months / 3,
            "months" => self.months,
            "monthsOfYear" => self.months % 12,
            "monthsOfQuarter" => self.months % 3,
            "quartersOfYear" => self.months / 3 % 4,
            "weeks" => self.days / 7,
            "days" => self.days,
            "daysOfWeek" => self.days % 7,
            "hours" => ts / 3600,
            "minutes" => ts / 60,
            "minutesOfHour" => ts / 60 % 60,
            "seconds" => ts,
            "secondsOfMinute" => ts % 60,
            "milliseconds" => ts * 1000 + tn / 1_000_000,
            "microseconds" => ts * 1_000_000 + tn / 1000,
            "nanoseconds" => ts * NANOS_PER_SECOND + tn,
            "millisecondsOfSecond" => tn / 1_000_000,
            "microsecondsOfSecond" => tn / 1000,
            "nanosecondsOfSecond" => tn,
            _ => return None,
        };
        Some(Value::Int(v))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("P")?;
        let (y, m) = (self.months / 12, self.months % 12);
        if y != 0 {
            write!(f, "{y}Y")?;
        }
        if m != 0 {
            write!(f, "{m}M")?;
        }
        if self.days != 0 {
            write!(f, "{}D", self.days)?;
        }
        let total = self.total_nanos();
        if total == 0 {
            if self.months == 0 && self.days == 0 {
                f.write_str("T0S")?;
            }
            return Ok(());
        }
        f.write_str("T")?;
        let neg = total < 0;
        let a = total.unsigned_abs();
        let ns = NANOS_PER_SECOND as u128;
        let (h, mi, s, frac) = (a / (3600 * ns), a / (60 * ns) % 60, a / ns % 60, (a % ns) as i64);
        let sign = if neg { "-" } else { "" };
        if h != 0 {
            write!(f, "{sign}{h}H")?;
        }
        if mi != 0 {
            write!(f, "{sign}{mi}M")?;
        }
        if s != 0 || frac != 0 {
            write!(f, "{sign}{s}")?;
            if frac != 0 {
                // All the digits a duration needs, no trailing zeros: `0.5S`, `0.001S`.
                let digits = format!("{frac:09}");
                write!(f, ".{}", digits.trim_end_matches('0'))?;
            }
            f.write_str("S")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: TemporalKind, s: &str) -> Temporal {
        Temporal::parse(kind, s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn calendar_round_trips() {
        for days in [-1_000_000, -719_468, -1, 0, 1, 10_957, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(weekday(0), 4); // a Thursday
        assert_eq!(iso_week(days_from_civil(2021, 1, 3)), (2020, 53));
        assert_eq!(iso_week(days_from_civil(2024, 12, 30)), (2025, 1));
    }

    #[test]
    fn parses_and_formats() {
        use TemporalKind as K;
        for (k, text, shown) in [
            (K::Date, "2015-07-21", "2015-07-21"),
            (K::Date, "20150721", "2015-07-21"),
            (K::Date, "2015-07", "2015-07-01"),
            (K::Date, "2015", "2015-01-01"),
            (K::Date, "2015-W30-2", "2015-07-21"),
            (K::Date, "2015W302", "2015-07-21"),
            (K::Date, "2015-202", "2015-07-21"),
            (K::Date, "-0044-03-15", "-0044-03-15"),
            (K::Date, "+12345-01-01", "+12345-01-01"),
            (K::LocalTime, "12:31:14.645876123", "12:31:14.645876123"),
            (K::LocalTime, "123114.5", "12:31:14.500"),
            (K::LocalTime, "12:31", "12:31"),
            (K::LocalTime, "12", "12:00"),
            (K::Time, "12:31:14+01:00", "12:31:14+01:00"),
            (K::Time, "12:31:14", "12:31:14Z"),
            (K::Time, "12:31:14-0530", "12:31:14-05:30"),
            (K::LocalDateTime, "2015-07-21T21:40:32.142", "2015-07-21T21:40:32.142"),
            (K::LocalDateTime, "2015-07-21", "2015-07-21T00:00"),
            (K::DateTime, "2015-07-21T21:40:32.142+01:00", "2015-07-21T21:40:32.142+01:00"),
            (K::DateTime, "2015-07-21T21:40Z", "2015-07-21T21:40Z"),
            (K::Duration, "P14DT16H12M", "P14DT16H12M"),
            (K::Duration, "P1Y2M10DT2H30M15.5S", "P1Y2M10DT2H30M15.5S"),
            (K::Duration, "PT0S", "PT0S"),
            (K::Duration, "P0.5D", "PT12H"),
            (K::Duration, "P1W", "P7D"),
            (K::Duration, "P1.5Y", "P1Y6M"),
            (K::Duration, "P0.75M", "P22DT19H51M49.5S"),
            (K::Duration, "P2.5W", "P17DT12H"),
            (K::Duration, "P2012-02-02T14:37:21.545", "P2012Y2M2DT14H37M21.545S"),
            (K::Duration, "PT-1.5S", "PT-1.5S"),
            (K::Duration, "-P1D", "P-1D"),
        ] {
            assert_eq!(p(k, text).to_string(), shown, "{text}");
        }
        for (k, bad) in [
            (K::Date, "2015-02-29"),
            (K::Date, "2015-13-01"),
            (K::Date, "15-01-01"),
            (K::Date, "2015-W54"),
            (K::LocalTime, "24:00"),
            (K::LocalTime, "12:00+01:00"),
            (K::LocalDateTime, "2015-01-01T00:00Z"),
            (K::Duration, "P"),
            (K::Duration, "PT1D"),
        ] {
            assert!(Temporal::parse(k, bad).is_err(), "{bad}");
        }
        for (text, shown) in [
            ("2015-07-21T21:40:32[Europe/Paris]", "2015-07-21T21:40:32+02:00[Europe/Paris]"),
            ("2015-07-21T21:40:32.142+0845[Australia/Eucla]", "2015-07-21T21:40:32.142+08:45[Australia/Eucla]"),
            ("1818-07-21T21:40:32.142[Europe/Stockholm]", "1818-07-21T21:40:32.142+00:53:28[Europe/Stockholm]"),
            // In a gap the clock moves on; in an overlap the given offset is kept.
            ("2017-03-26T02:30[Europe/Stockholm]", "2017-03-26T03:30+02:00[Europe/Stockholm]"),
            ("2017-10-29T02:30+01:00[Europe/Stockholm]", "2017-10-29T02:30+01:00[Europe/Stockholm]"),
            ("2017-10-29T02:30[Europe/Stockholm]", "2017-10-29T02:30+02:00[Europe/Stockholm]"),
        ] {
            assert_eq!(p(K::DateTime, text).to_string(), shown, "{text}");
        }
        assert!(Temporal::parse(K::DateTime, "2015-07-21T21:40[Frost/Nixon]").is_err());
    }

    #[test]
    fn arithmetic_clamps_and_carries() {
        use TemporalKind as K;
        let d = |s| match p(K::Duration, s) {
            Temporal::Duration(d) => d,
            _ => unreachable!(),
        };
        let plus = |k, a, b| p(k, a).plus(&d(b)).unwrap().to_string();
        assert_eq!(plus(K::Date, "2020-01-31", "P1M"), "2020-02-29");
        assert_eq!(plus(K::Date, "2020-01-31", "P1M1D"), "2020-03-01");
        assert_eq!(plus(K::Date, "2020-01-01", "PT47H"), "2020-01-02");
        assert_eq!(plus(K::LocalTime, "23:00", "PT2H"), "01:00");
        assert_eq!(plus(K::LocalDateTime, "2020-12-31T23:00", "PT2H"), "2021-01-01T01:00");
        assert_eq!(plus(K::DateTime, "2020-03-01T00:00+02:00", "-PT1S"), "2020-02-29T23:59:59+02:00");
        assert_eq!(d("P1DT1H").scale(1.5).unwrap().to_string(), "P1DT13H30M");
        assert_eq!(d("P1M").scale(0.5).unwrap().to_string(), "P15DT5H14M33S");
    }

    #[test]
    fn between() {
        use TemporalKind as K;
        let b = |a, z, u| Temporal::between(&p(K::LocalDateTime, a), &p(K::LocalDateTime, z), u).unwrap().to_string();
        assert_eq!(b("2020-01-31T10:00", "2020-03-01T09:00", BetweenUnit::All), "P1MT23H");
        assert_eq!(b("2020-01-31T10:00", "2020-03-01T09:00", BetweenUnit::Days), "P29D");
        assert_eq!(b("2020-03-01", "2020-01-31", BetweenUnit::All), "P-1M-1D");
        assert_eq!(b("2020-01-01", "2020-01-02T00:00:00.5", BetweenUnit::Seconds), "PT24H0.5S");
        let z = Temporal::between(
            &p(K::DateTime, "2020-01-01T00:00+01:00"),
            &p(K::DateTime, "2020-01-01T00:00Z"),
            BetweenUnit::All,
        )
        .unwrap();
        assert_eq!(z.to_string(), "PT1H");
    }

    #[test]
    fn equality_and_order() {
        use TemporalKind as K;
        let a = p(K::DateTime, "2020-01-01T01:00+01:00");
        let b = p(K::DateTime, "2020-01-01T00:00Z");
        assert!(a.cypher_eq(&b));
        assert_ne!(a, b);
        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(a.cypher_cmp(&p(K::DateTime, "2020-01-01T00:30Z")), Some(Ordering::Less));
        assert_eq!(p(K::Duration, "P1D").cypher_cmp(&p(K::Duration, "P2D")), None);
        assert_eq!(p(K::Duration, "P1M").order_cmp(&p(K::Duration, "P30D")), Ordering::Greater);
        assert_eq!(p(K::Date, "2020-01-01").cypher_cmp(&p(K::LocalDateTime, "2020-01-01")), None);
    }

    #[test]
    fn fields() {
        use TemporalKind as K;
        let t = p(K::DateTime, "2015-07-21T21:40:32.142+01:00");
        let f = |k: &str| t.field(k).unwrap().to_string();
        assert_eq!(
            ["year", "quarter", "month", "week", "day", "ordinalDay", "dayOfWeek", "hour", "millisecond", "offset"]
                .map(f),
            ["2015", "3", "7", "30", "21", "202", "2", "21", "142", "\"+01:00\""]
        );
        assert_eq!(f("epochSeconds"), "1437511232");
        let d = p(K::Duration, "P1Y14M3DT25H61M-1.5S");
        let g = |k: &str| d.field(k).unwrap().to_string();
        assert_eq!(
            ["years", "months", "monthsOfYear", "days", "hours", "minutesOfHour", "seconds", "milliseconds"].map(g),
            ["2", "26", "2", "3", "26", "0", "93658", "93658500"]
        );
        assert!(t.field("days").is_none());
    }
}
