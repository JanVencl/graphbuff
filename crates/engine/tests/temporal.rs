//! Temporal values (`date`, `localtime`, `time`, `localdatetime`, `datetime`,
//! `duration`), end to end: constructors, fields, arithmetic, comparison and ordering,
//! and storage (with indexes) on every store.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Database, Error, QueryResult, StorageMode};

type Db = Database<BoxedStore>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-ttest-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `test` on a fresh database of every store kind; `reopen` closes and reopens it.
fn on_every_store(test: impl Fn(&str, Db, &dyn Fn(Db) -> Db)) {
    test("memory", Database::open(StorageMode::InMemory).unwrap(), &|db| db);
    for kind in ["hybrid", "persistent"] {
        let dir = TempDir::new(kind);
        let mode = || match kind {
            "hybrid" => StorageMode::hybrid(&dir.0),
            _ => StorageMode::persistent(&dir.0),
        };
        test(kind, Database::open(mode()).unwrap(), &|db| {
            drop(db);
            Database::open(mode()).unwrap()
        });
    }
}

fn run(db: &Db, q: &str) -> QueryResult {
    db.execute(q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn rows(db: &Db, q: &str) -> Vec<String> {
    run(db, q)
        .rows
        .iter()
        .map(|r| r.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect()
}

/// The single value `RETURN <expr>` gives, rendered.
fn eval(db: &Db, expr: &str) -> String {
    let r = rows(db, &format!("RETURN {expr}"));
    assert_eq!(r.len(), 1, "{expr}");
    r[0].clone()
}

fn mem() -> Db {
    Database::open(StorageMode::InMemory).unwrap()
}

#[test]
fn constructors_and_fields() {
    let db = mem();
    for (expr, want) in [
        ("date('2015-07-21')", "2015-07-21"),
        ("date({year: 1984, month: 10, day: 11})", "1984-10-11"),
        ("date({year: 1984, week: 10, dayOfWeek: 3})", "1984-03-07"),
        ("date({year: 1984, ordinalDay: 202})", "1984-07-20"),
        ("date({year: 1984, quarter: 3, dayOfQuarter: 45})", "1984-08-14"),
        ("date(datetime('2015-07-21T23:00-05:00'))", "2015-07-21"),
        ("date({date: date('2015-07-21'), day: 1})", "2015-07-01"),
        ("localtime('12:31:14.645')", "12:31:14.645"),
        ("localtime({hour: 12, minute: 31, second: 14, millisecond: 645})", "12:31:14.645"),
        ("time('12:31:14+01:00')", "12:31:14+01:00"),
        ("time({hour: 12, timezone: '-05:30'})", "12:00:00-05:30"),
        ("localdatetime('2015-07-21T21:40:32')", "2015-07-21T21:40:32"),
        ("localdatetime({date: date('2015-07-21'), time: localtime('10:00')})", "2015-07-21T10:00:00"),
        ("datetime('2015-07-21T21:40:32.142+01:00')", "2015-07-21T21:40:32.142+01:00"),
        ("datetime({year: 2015, month: 7, day: 21, hour: 21, timezone: 'Z'})", "2015-07-21T21:00:00Z"),
        ("datetime({datetime: datetime('2015-07-21T21:00Z'), timezone: '+02:00'})", "2015-07-21T23:00:00+02:00"),
        ("datetime({epochSeconds: 1437511232})", "2015-07-21T20:40:32Z"),
        ("datetime.fromepoch(1437511232, 5000000)", "2015-07-21T20:40:32.005Z"),
        ("datetime.fromepochmillis(1437511232142)", "2015-07-21T20:40:32.142Z"),
        ("duration('P1Y2M10DT12H45M30.25S')", "P1Y2M10DT12H45M30.250S"),
        ("duration({days: 14, hours: 16, minutes: 12})", "P14DT16H12M"),
        ("duration({months: 0.75})", "P22DT19H51M49.500S"),
        ("date(null)", "null"),
        ("toString(date('2015-07-21'))", "\"2015-07-21\""),
        // Fields.
        ("date('2015-07-21').year", "2015"),
        ("date('2015-07-21').week", "30"),
        ("date('2015-07-21').dayOfWeek", "2"),
        ("date('2015-07-21').ordinalDay", "202"),
        ("datetime('2015-07-21T21:40:32.142+01:00').hour", "21"),
        ("datetime('2015-07-21T21:40:32.142+01:00').timezone", "\"+01:00\""),
        ("datetime('2015-07-21T21:40:32.142+01:00').offsetMinutes", "60"),
        ("datetime('2015-07-21T21:40:32.142+01:00').epochMillis", "1437511232142"),
        ("time('12:00-02:00').offsetSeconds", "-7200"),
        ("duration('P1Y3M5DT7H').months", "15"),
        ("duration('P1Y3M5DT7H').monthsOfYear", "3"),
        ("duration('PT90M').hours", "1"),
        ("duration('PT90M').minutesOfHour", "30"),
    ] {
        assert_eq!(eval(&db, expr), want, "{expr}");
    }
    // Every call in a statement sees the same clock.
    assert_eq!(eval(&db, "datetime() = datetime()"), "true");
    assert_eq!(eval(&db, "date() = date(datetime.statement())"), "true");

    for (expr, kind) in [
        ("date('2015-02-29')", "runtime"),
        ("date({month: 2})", "runtime"),
        ("date({year: 2015, colour: 'red'})", "runtime"),
        ("localtime('12:00+01:00')", "runtime"),
        ("date(42)", "type"),
        ("datetime('2015-07-21T21:40[Europe/Paris]')", "unsupported"),
        ("date('2015-07-21').hour", "semantic"),
        ("date('2015-01-01') < localdatetime('2015-01-01')", "ok"),
    ] {
        let r = db.execute(&format!("RETURN {expr}"));
        let ok = matches!(
            (kind, &r),
            ("runtime", Err(Error::Runtime(_)))
                | ("type", Err(Error::Type(_)))
                | ("unsupported", Err(Error::Unsupported(_)))
                | ("semantic", Err(Error::Semantic(_)))
                | ("ok", Ok(_))
        );
        assert!(ok, "{expr}: expected {kind}, got {r:?}");
    }
    // Different kinds do not compare: the result is null.
    assert_eq!(eval(&db, "date('2015-01-01') < localdatetime('2015-01-01')"), "null");
}

#[test]
fn arithmetic_and_comparison() {
    let db = mem();
    for (expr, want) in [
        ("date('2020-01-31') + duration('P1M')", "2020-02-29"),
        ("duration('P1M') + date('2020-01-31')", "2020-02-29"),
        ("date('2020-03-31') - duration('P1M')", "2020-02-29"),
        ("date('2020-01-01') + duration('PT36H')", "2020-01-02"),
        ("localtime('23:30') + duration('PT1H')", "00:30:00"),
        ("localdatetime('2020-12-31T23:30') + duration('PT1H')", "2021-01-01T00:30:00"),
        ("datetime('2020-03-01T00:00+02:00') - duration('PT1S')", "2020-02-29T23:59:59+02:00"),
        ("duration('P1D') + duration('PT12H')", "P1DT12H"),
        ("duration('P1D') - duration('PT12H')", "P1DT-12H"),
        ("duration('PT1H') * 2.5", "PT2H30M"),
        ("3 * duration('P1D')", "P3D"),
        ("duration('P1DT1H') / 2", "PT12H30M"),
        ("duration.between(date('2020-01-31'), date('2020-03-01'))", "P1M1D"),
        ("duration.between(localdatetime('2020-01-01T12:00'), localdatetime('2020-01-03T06:00'))", "P1DT18H"),
        ("duration.inMonths(date('2020-01-31'), date('2021-03-01'))", "P1Y1M"),
        ("duration.inDays(date('2020-01-01'), date('2021-01-01'))", "P366D"),
        ("duration.inSeconds(localtime('10:00'), localtime('12:30'))", "PT2H30M"),
        ("duration.between(datetime('2020-01-01T00:00+01:00'), datetime('2020-01-01T00:00Z'))", "PT1H"),
        ("duration.between(null, date())", "null"),
        ("date('2020-01-01') < date('2020-01-02')", "true"),
        ("datetime('2020-01-01T01:00+01:00') = datetime('2020-01-01T00:00Z')", "true"),
        ("datetime('2020-01-01T01:00+01:00') < datetime('2020-01-01T00:30Z')", "true"),
        ("time('10:00+02:00') = time('08:00Z')", "true"),
        ("duration('P1D') = duration('PT24H')", "false"),
        ("duration('P1D') < duration('P2D')", "null"),
        ("date('2020-01-01') = '2020-01-01'", "false"),
        ("date('2020-01-01') IN [date('2020-01-01')]", "true"),
    ] {
        assert_eq!(eval(&db, expr), want, "{expr}");
    }
    assert!(db.execute("RETURN date('2020-01-01') + date('2020-01-01')").is_err());
    assert!(db.execute("RETURN duration('P1D') / 0").is_err());
    assert!(db.execute("RETURN date('2020-01-01') - date('2019-01-01')").is_err());
    assert_eq!(
        rows(
            &db,
            "UNWIND [duration('P1M'), date('2020-01-01'), localtime('10:00'), datetime('2020-01-01T00:00Z'),
                     localdatetime('2020-01-01T00:00'), time('10:00Z'), 'a', 1, null,
                     date('2019-12-31'), duration('P30D')] AS v
             RETURN v ORDER BY v"
        ),
        [
            "2020-01-01T00:00:00Z", "2020-01-01T00:00:00", "2019-12-31", "2020-01-01", "10:00:00Z",
            "10:00:00", "P30D", "P1M", "\"a\"", "1", "null",
        ]
    );
    assert_eq!(
        rows(&db, "UNWIND [date('2020-01-02'), date('2020-01-01')] AS d RETURN min(d), max(d)"),
        ["2020-01-01, 2020-01-02"]
    );
    assert_eq!(
        rows(
            &db,
            "UNWIND [datetime('2020-01-01T01:00+01:00'), datetime('2020-01-01T00:00Z')] AS d
             RETURN count(DISTINCT d)"
        ),
        ["2"],
        "DISTINCT keeps both offsets (like 1 and 1.0)"
    );
}

#[test]
fn stored_temporals_and_indexes() {
    on_every_store(|store, db, reopen| {
        run(
            &db,
            "UNWIND range(0, 99) AS i
             CREATE (:Ev {i: i,
                          on: date('2024-01-01') + duration({days: i}),
                          at: datetime('2024-01-01T00:00Z') + duration({hours: i}),
                          t: localtime('00:00') + duration({minutes: i}),
                          len: duration({minutes: i}),
                          tags: [date('2024-01-01'), localdatetime('2024-01-01T10:00')]})",
        );
        run(&db, "CREATE INDEX FOR (e:Ev) ON (e.at)");
        run(&db, "CREATE INDEX FOR (e:Ev) ON (e.on)");
        let db = reopen(db);
        let q = |q: &str| rows(&db, q);
        assert_eq!(q("MATCH (e:Ev {i: 31}) RETURN e.on, e.at, e.t, e.len"), ["2024-02-01, 2024-01-02T07:00:00Z, 00:31:00, PT31M"], "{store}");
        assert_eq!(q("MATCH (e:Ev {i: 0}) RETURN e.tags"), ["[2024-01-01, 2024-01-01T10:00:00]"], "{store}");
        // An index seek finds the same instant at another offset.
        let seek = "MATCH (e:Ev) WHERE e.at = datetime('2024-01-02T09:00+02:00') RETURN e.i";
        assert!(run(&db, &format!("EXPLAIN {seek}")).plan.unwrap().to_string().contains("NodeIndexSeek"), "{store}");
        assert_eq!(q(seek), ["31"], "{store}");
        assert_eq!(q("MATCH (e:Ev {on: date('2024-04-09')}) RETURN e.i"), ["99"], "{store}");
        assert_eq!(q("MATCH (e:Ev) WHERE e.on > date('2024-04-07') RETURN e.i ORDER BY e.i"), ["98", "99"], "{store}");
        assert_eq!(q("MATCH (e:Ev) WHERE e.len >= duration('PT1M') RETURN count(e)"), ["0"], "{store}: durations do not compare");
        assert_eq!(q("MATCH (e:Ev) RETURN max(e.at)"), ["2024-01-05T03:00:00Z"], "{store}");
        // Updates, then reopen again.
        run(&db, "MATCH (e:Ev) WHERE e.i < 10 SET e.at = e.at + duration('P1Y')");
        let db = reopen(db);
        assert_eq!(rows(&db, "MATCH (e:Ev) WHERE e.at.year = 2025 RETURN count(e)"), ["10"], "{store}");
        assert_eq!(
            rows(&db, "MATCH (e:Ev {at: datetime('2025-01-01T05:00Z')}) RETURN e.i"),
            ["5"],
            "{store}: the index follows updates"
        );
        // Maps and other non-storable values still are not storable.
        assert!(db.execute("CREATE (:Ev {m: {d: date()}})").is_err());
    });
}
