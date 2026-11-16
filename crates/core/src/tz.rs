//! Named time zones (`Europe/Stockholm`) from the IANA time zone database, embedded as a
//! compact table (`tzdata/tzdata.bin`, built by `tzdata/generate.py`): per zone, its UTC
//! offset changes, and a POSIX TZ rule (`CET-1CEST,M3.5.0,M10.5.0/3`) for the years
//! after the last one.

use std::sync::OnceLock;

use crate::temporal::days_from_civil;

static TABLE: &[u8] = include_bytes!("../tzdata/tzdata.bin");

/// A named time zone: an index into the table's zone names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tz(u16);

struct Zone {
    /// UTC second of each change, ascending, and the offset (seconds east) from then on.
    changes: Vec<(i64, i32)>,
    /// The offset before the first change.
    initial: i32,
    rule: Option<Rule>,
}

struct Db {
    version: String,
    /// Zone names, sorted, with the index of their data.
    names: Vec<(String, usize)>,
    zones: Vec<Zone>,
}

fn db() -> &'static Db {
    static DB: OnceLock<Db> = OnceLock::new();
    DB.get_or_init(|| parse_table(TABLE).expect("the embedded time zone table is valid"))
}

fn parse_table(b: &[u8]) -> Option<Db> {
    let mut r = Reader { b, at: 0 };
    if r.take(4)? != b"GBTZ" || r.u8()? != 1 {
        return None;
    }
    let version = r.str()?;
    let count = r.u16()? as usize;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let name = r.str()?;
        names.push((name, r.u16()? as usize));
    }
    let datas = r.u16()? as usize;
    let mut zones = Vec::with_capacity(datas);
    for _ in 0..datas {
        let n = r.u8()? as usize;
        let offsets = (0..n).map(|_| r.i32()).collect::<Option<Vec<_>>>()?;
        let initial = *offsets.get(r.u8()? as usize)?;
        let t = r.u32()? as usize;
        let times = (0..t).map(|_| r.i64()).collect::<Option<Vec<_>>>()?;
        let idx = r.take(t)?;
        let changes = times
            .into_iter()
            .zip(idx)
            .map(|(t, &i)| offsets.get(i as usize).map(|&o| (t, o)))
            .collect::<Option<Vec<_>>>()?;
        let rule = r.str()?;
        let rule = if rule.is_empty() { None } else { Some(Rule::parse(&rule)?) };
        zones.push(Zone { changes, initial, rule });
    }
    Some(Db { version, names, zones })
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at + n)?;
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

impl Tz {
    /// The zone named `name` (exactly, else ignoring case).
    pub fn find(name: &str) -> Option<Tz> {
        let names = &db().names;
        let i = match names.binary_search_by(|(n, _)| n.as_str().cmp(name)) {
            Ok(i) => i,
            Err(_) => names.iter().position(|(n, _)| n.eq_ignore_ascii_case(name))?,
        };
        Some(Tz(i as u16))
    }

    /// The zone's name, as the database spells it.
    pub fn name(self) -> &'static str {
        &db().names[self.0 as usize].0
    }

    /// The release of the time zone database (`2026f`).
    pub fn database_version() -> &'static str {
        &db().version
    }

    fn zone(self) -> &'static Zone {
        let db = db();
        &db.zones[db.names[self.0 as usize].1]
    }

    /// The UTC offset (seconds east) at the instant `utc` (seconds since the epoch).
    pub fn offset_at(self, utc: i64) -> i32 {
        let z = self.zone();
        match z.changes.partition_point(|&(t, _)| t <= utc) {
            0 => z.initial,
            n if n == z.changes.len() => match &z.rule {
                Some(rule) => rule.offset_at(utc),
                None => z.changes[n - 1].1,
            },
            n => z.changes[n - 1].1,
        }
    }

    /// The offset of the local time `local` (seconds since the epoch, in wall-clock
    /// time), and `local` moved past a gap if it falls in one (as Java does: by the
    /// gap's length). In an overlap, `prefer` wins if it is one of the two offsets,
    /// else the earlier one (the offset before the change).
    pub fn resolve(self, local: i64, prefer: Option<i32>) -> (i64, i32) {
        // At most one change within a day either side, in practice.
        let before = self.offset_at(local - 86_400);
        let after = self.offset_at(local + 86_400);
        let valid = |o: i32| self.offset_at(local - o as i64) == o;
        let mut fits: Vec<i32> = [before, after].into_iter().filter(|&o| valid(o)).collect();
        fits.dedup();
        match fits.as_slice() {
            [] => {
                // A gap: before → after moves the clocks forward.
                let shifted = local + (after - before) as i64;
                (shifted, after)
            }
            [o] => (local, *o),
            _ => match prefer {
                Some(p) if fits.contains(&p) => (local, p),
                _ => (local, fits[0]),
            },
        }
    }
}

// ---- POSIX TZ rules -------------------------------------------------------------------

/// A POSIX TZ rule: a standard offset, and maybe a daylight offset with the dates it
/// starts and ends each year.
struct Rule {
    std: i32,
    dst: Option<(i32, RuleDate, i64, RuleDate, i64)>,
}

/// When a rule switches, within a year.
#[derive(Clone, Copy)]
enum RuleDate {
    /// `Jn`: day 1..=365, February 29 never counted.
    Julian(i64),
    /// `n`: day 0..=365, counting February 29.
    Zero(i64),
    /// `Mm.w.d`: weekday `d` (0 = Sunday) of week `w` (5 = the last) of month `m`.
    Month(i64, i64, i64),
}

impl Rule {
    fn parse(s: &str) -> Option<Rule> {
        let mut c = s.as_bytes();
        name(&mut c)?;
        // POSIX offsets count west of UTC.
        let std = -signed_time(&mut c)? as i32;
        if c.is_empty() {
            return Some(Rule { std, dst: None });
        }
        name(&mut c)?;
        let dst = if c.first().is_some_and(|b| *b != b',') { -signed_time(&mut c)? as i32 } else { std + 3600 };
        let mut part = || -> Option<(RuleDate, i64)> {
            if c.first() != Some(&b',') {
                return None;
            }
            c = &c[1..];
            let date = match c.first()? {
                b'J' => {
                    c = &c[1..];
                    RuleDate::Julian(number(&mut c)?)
                }
                b'M' => {
                    c = &c[1..];
                    let m = number(&mut c)?;
                    eat(&mut c, b'.')?;
                    let w = number(&mut c)?;
                    eat(&mut c, b'.')?;
                    RuleDate::Month(m, w, number(&mut c)?)
                }
                _ => RuleDate::Zero(number(&mut c)?),
            };
            let time = if c.first() == Some(&b'/') {
                c = &c[1..];
                signed_time(&mut c)?
            } else {
                7200
            };
            Some((date, time))
        };
        let (start, start_t) = part()?;
        let (end, end_t) = part()?;
        if !c.is_empty() {
            return None;
        }
        Some(Rule { std, dst: Some((dst, start, start_t, end, end_t)) })
    }

    fn offset_at(&self, utc: i64) -> i32 {
        let Some((dst, start, start_t, end, end_t)) = self.dst else {
            return self.std;
        };
        let year = crate::temporal::civil_from_days((utc + self.std as i64).div_euclid(86_400)).0;
        // The switches of `year`, in UTC: the start is in standard time, the end in daylight.
        let starts = day_of(year, start) * 86_400 + start_t - self.std as i64;
        let ends = day_of(year, end) * 86_400 + end_t - dst as i64;
        let in_dst = if starts < ends { starts <= utc && utc < ends } else { !(ends <= utc && utc < starts) };
        if in_dst { dst } else { self.std }
    }
}

/// Days since the epoch of `d` in `year`.
fn day_of(year: i64, d: RuleDate) -> i64 {
    let jan1 = days_from_civil(year, 1, 1);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match d {
        RuleDate::Julian(n) => jan1 + n - 1 + i64::from(leap && n >= 60),
        RuleDate::Zero(n) => jan1 + n,
        RuleDate::Month(m, w, wd) => {
            let first = days_from_civil(year, m as u32, 1);
            // 1970-01-01 was a Thursday (4).
            let first_wd = (first + 4).rem_euclid(7);
            let mut day = first + (wd - first_wd).rem_euclid(7) + (w - 1) * 7;
            let next = if m == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, m as u32 + 1, 1) };
            while day >= next {
                day -= 7;
            }
            day
        }
    }
}

fn name(c: &mut &[u8]) -> Option<()> {
    if c.first() == Some(&b'<') {
        let end = c.iter().position(|&b| b == b'>')?;
        *c = &c[end + 1..];
    } else {
        let n = c.iter().take_while(|b| b.is_ascii_alphabetic()).count();
        if n < 3 {
            return None;
        }
        *c = &c[n..];
    }
    Some(())
}

fn eat(c: &mut &[u8], b: u8) -> Option<()> {
    if c.first() == Some(&b) {
        *c = &c[1..];
        Some(())
    } else {
        None
    }
}

fn number(c: &mut &[u8]) -> Option<i64> {
    let n = c.iter().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let v = std::str::from_utf8(&c[..n]).ok()?.parse().ok()?;
    *c = &c[n..];
    Some(v)
}

/// `[+-]hh[:mm[:ss]]` in seconds.
fn signed_time(c: &mut &[u8]) -> Option<i64> {
    let sign = match c.first() {
        Some(b'-') => {
            *c = &c[1..];
            -1
        }
        Some(b'+') => {
            *c = &c[1..];
            1
        }
        _ => 1,
    };
    let mut secs = number(c)? * 3600;
    for unit in [60, 1] {
        if c.first() != Some(&b':') {
            break;
        }
        *c = &c[1..];
        secs += number(c)? * unit;
    }
    Some(sign * secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i64, m: u32, d: u32, h: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600
    }

    #[test]
    fn offsets() {
        let sto = Tz::find("Europe/Stockholm").unwrap();
        assert_eq!(sto.name(), "Europe/Stockholm");
        assert_eq!(Tz::find("europe/stockholm"), Some(sto));
        assert!(Tz::find("Frost/Nixon").is_none());
        assert_eq!(sto.offset_at(utc(1984, 10, 11, 12)), 3600);
        assert_eq!(sto.offset_at(utc(1984, 7, 20, 12)), 7200);
        assert_eq!(sto.offset_at(utc(1818, 7, 21, 12)), 3208); // local mean time, +00:53:28
        // Past the table, from the rule: last Sunday of March at 01:00 UTC.
        assert_eq!(sto.offset_at(utc(2100, 3, 28, 0)), 3600);
        assert_eq!(sto.offset_at(utc(2100, 3, 28, 1)), 7200);
        assert_eq!(sto.offset_at(utc(2100, 10, 31, 0)), 7200);
        assert_eq!(sto.offset_at(utc(2100, 10, 31, 1)), 3600);
        assert_eq!(Tz::find("Australia/Eucla").unwrap().offset_at(utc(2015, 7, 21, 0)), 8 * 3600 + 45 * 60);
        assert_eq!(Tz::find("Pacific/Honolulu").unwrap().offset_at(utc(1984, 10, 28, 0)), -10 * 3600);
        // Southern hemisphere: daylight time spans the new year.
        let syd = Tz::find("Australia/Sydney").unwrap();
        assert_eq!(syd.offset_at(utc(2090, 1, 15, 0)), 11 * 3600);
        assert_eq!(syd.offset_at(utc(2090, 7, 15, 0)), 10 * 3600);
    }

    #[test]
    fn local_times() {
        let sto = Tz::find("Europe/Stockholm").unwrap();
        let local = |y, m, d, h| utc(y, m, d, h);
        // A gap: 2017-03-26 02:30 does not exist, it becomes 03:30 +02:00.
        assert_eq!(sto.resolve(local(2017, 3, 26, 2) + 1800, None), (local(2017, 3, 26, 3) + 1800, 7200));
        // An overlap: 2017-10-29 02:30 happens twice; the earlier offset wins unless preferred.
        assert_eq!(sto.resolve(local(2017, 10, 29, 2) + 1800, None).1, 7200);
        assert_eq!(sto.resolve(local(2017, 10, 29, 2) + 1800, Some(3600)).1, 3600);
        assert_eq!(sto.resolve(local(2017, 10, 29, 12), Some(7200)).1, 3600);
    }
}
