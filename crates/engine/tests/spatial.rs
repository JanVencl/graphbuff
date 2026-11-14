//! Spatial values (`point`), end to end: construction, fields, `point.distance`,
//! `point.withinBBox`, equality and ordering, and storage (with indexes) on every store.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Crs, Database, Error, QueryResult, StorageMode};

type Db = Database<BoxedStore>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-stest-{}-{}-{name}",
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
    let e = |x: &str| eval(&db, x);
    assert_eq!(e("point({x: 1, y: 2.5})"), "point({x: 1.0, y: 2.5, crs: 'cartesian'})");
    assert_eq!(e("point({x: 1, y: 2, z: 3})"), "point({x: 1.0, y: 2.0, z: 3.0, crs: 'cartesian-3d'})");
    assert_eq!(e("point({longitude: 12.5, latitude: 56})"), "point({x: 12.5, y: 56.0, crs: 'wgs-84'})");
    assert_eq!(
        e("point({longitude: 12.5, latitude: 56, height: 100})"),
        "point({x: 12.5, y: 56.0, z: 100.0, crs: 'wgs-84-3d'})"
    );
    assert_eq!(e("point({x: 1, y: 2, crs: 'WGS-84'}).srid"), "4326");
    assert_eq!(e("point({x: 1, y: 2, srid: 4326}).latitude"), "2.0");
    assert_eq!(e("point({x: 1, y: 2, z: 3, srid: 9157}).crs"), "\"cartesian-3d\"");
    let p = "point({longitude: 12.5, latitude: 56, height: 100})";
    assert_eq!(
        e(&format!("[{p}.x, {p}.y, {p}.z, {p}.longitude, {p}.latitude, {p}.height, {p}.srid]")),
        "[12.5, 56.0, 100.0, 12.5, 56.0, 100.0, 4979]"
    );
    assert_eq!(e("point(null)"), "null");
    assert_eq!(e("point({x: null, y: 2})"), "null");
    assert_eq!(e("toString(point({x: 1, y: 2}))"), "\"point({x: 1.0, y: 2.0, crs: 'cartesian'})\"");

    let r = run(&db, "RETURN point({x: 3, y: 4, crs: 'cartesian'}) AS p");
    let p = r.rows[0][0].as_point().unwrap();
    assert_eq!((p.crs(), p.coords()), (Crs::Cartesian, &[3.0, 4.0][..]));

    for bad in [
        "point({x: 1})",
        "point({x: 1, latitude: 2})",
        "point({x: 1, y: 2, crs: 'cartesian-3d'})",
        "point({x: 1, y: 2, crs: 'mars'})",
        "point({x: 1, y: 2, srid: 1234})",
        "point({longitude: 0, latitude: 90.5})",
        "point({longitude: 180.5, latitude: 0})",
        "point({x: 1, y: 'a'})",
        "point({x: 1, y: 2, w: 3})",
        "point(1)",
        "point({x: 1, y: 2}).latitude",
        "point({x: 1, y: 2}).z",
        "point.nosuch(1)",
    ] {
        assert!(db.execute(&format!("RETURN {bad}")).is_err(), "{bad}");
    }
    assert!(matches!(db.execute("RETURN point.nosuch(1)"), Err(Error::Unsupported(_))));
}

#[test]
fn distance_and_bbox() {
    let db = mem();
    let e = |x: &str| eval(&db, x);
    assert_eq!(e("point.distance(point({x: 0, y: 0}), point({x: 3, y: 4}))"), "5.0");
    assert_eq!(e("point.distance(point({x: 0, y: 0, z: 0}), point({x: 1, y: 2, z: 2}))"), "3.0");
    assert_eq!(e("point.distance(point({x: 0, y: 0}), point({x: 0, y: 0, z: 0}))"), "null");
    assert_eq!(e("point.distance(point({x: 0, y: 0}), point({longitude: 0, latitude: 0}))"), "null");
    assert_eq!(e("point.distance(null, point({x: 0, y: 0}))"), "null");
    // Two pairs of points in southern Sweden and Denmark (about 114 km and 29 km).
    let d: f64 = e("point.distance(point({longitude: 12.78, latitude: 56.7}), point({longitude: 12.56, latitude: 55.68}))")
        .parse()
        .unwrap();
    assert!((d - 114_000.0).abs() < 1_000.0, "{d}");
    let d: f64 = e("point.distance(point({longitude: 13.0, latitude: 55.6}), point({longitude: 12.56, latitude: 55.68}))")
        .parse()
        .unwrap();
    assert!((d - 28_800.0).abs() < 500.0, "{d}");
    // 3D geographic: the height difference alone.
    assert_eq!(e("point.distance(point({longitude: 1, latitude: 2, height: 0}), point({longitude: 1, latitude: 2, height: 40}))"), "40.0");
    assert!(db.execute("RETURN point.distance(1, 2)").is_err());

    let bbox = |p: &str, lo: &str, hi: &str| e(&format!("point.withinBBox({p}, {lo}, {hi})"));
    let c = |x: f64, y: f64| format!("point({{x: {x}, y: {y}}})");
    let g = |x: f64, y: f64| format!("point({{longitude: {x}, latitude: {y}}})");
    assert_eq!(bbox(&c(1.0, 1.0), &c(0.0, 0.0), &c(2.0, 2.0)), "true");
    assert_eq!(bbox(&c(2.0, 0.0), &c(0.0, 0.0), &c(2.0, 2.0)), "true");
    assert_eq!(bbox(&c(3.0, 1.0), &c(0.0, 0.0), &c(2.0, 2.0)), "false");
    assert_eq!(bbox(&g(179.0, 1.0), &g(170.0, 0.0), &g(-170.0, 2.0)), "true");
    assert_eq!(bbox(&g(0.0, 1.0), &g(170.0, 0.0), &g(-170.0, 2.0)), "false");
    assert_eq!(bbox(&c(1.0, 1.0), &g(0.0, 0.0), &g(2.0, 2.0)), "null");
    assert_eq!(bbox("null", &c(0.0, 0.0), &c(2.0, 2.0)), "null");
}

#[test]
fn equality_and_ordering() {
    let db = mem();
    let e = |x: &str| eval(&db, x);
    assert_eq!(e("point({x: 1, y: 2}) = point({x: 1.0, y: 2.0, crs: 'cartesian'})"), "true");
    assert_eq!(e("point({x: 0.0, y: 2}) = point({x: -0.0, y: 2})"), "true");
    assert_eq!(e("point({x: 1, y: 2}) = point({x: 1, y: 2, crs: 'wgs-84'})"), "false");
    assert_eq!(e("point({x: 1, y: 2}) <> point({x: 1, y: 3})"), "true");
    assert_eq!(e("point({x: 1, y: 2}) < point({x: 2, y: 3})"), "null", "points do not compare");
    assert_eq!(e("point({x: 1, y: 2}) IN [point({x: 1, y: 2})]"), "true");
    assert_eq!(
        rows(
            &db,
            "UNWIND [point({x: 2, y: 1}), 'a', point({longitude: 0, latitude: 0}), point({x: 1, y: 5}),
                     date('2024-01-01'), [1]] AS v
             RETURN v ORDER BY v"
        ),
        [
            "[1]",
            "point({x: 1.0, y: 5.0, crs: 'cartesian'})",
            "point({x: 2.0, y: 1.0, crs: 'cartesian'})",
            "point({x: 0.0, y: 0.0, crs: 'wgs-84'})",
            "2024-01-01",
            "\"a\"",
        ]
    );
    assert_eq!(
        rows(&db, "UNWIND [point({x: 1, y: 2}), point({x: 1.0, y: 2}), point({x: 2, y: 2})] AS p RETURN count(DISTINCT p)"),
        ["2"]
    );
}

#[test]
fn stored_points_and_indexes() {
    on_every_store(|store, db, reopen| {
        run(
            &db,
            "UNWIND range(0, 99) AS i
             CREATE (:Place {i: i,
                             at: point({x: i, y: i * 2}),
                             geo: point({longitude: i - 50, latitude: i / 2.0 - 25, height: i}),
                             route: [point({x: 0, y: 0}), point({x: i, y: 0})]})",
        );
        run(&db, "CREATE INDEX FOR (p:Place) ON (p.at)");
        let db = reopen(db);
        let q = |q: &str| rows(&db, q);
        assert_eq!(
            q("MATCH (p:Place {i: 31}) RETURN p.at, p.geo, p.route"),
            [
                "point({x: 31.0, y: 62.0, crs: 'cartesian'}), \
                 point({x: -19.0, y: -9.5, z: 31.0, crs: 'wgs-84-3d'}), \
                 [point({x: 0.0, y: 0.0, crs: 'cartesian'}), point({x: 31.0, y: 0.0, crs: 'cartesian'})]"
            ],
            "{store}"
        );
        let seek = "MATCH (p:Place) WHERE p.at = point({x: 7.0, y: 14}) RETURN p.i";
        assert!(run(&db, &format!("EXPLAIN {seek}")).plan.unwrap().to_string().contains("NodeIndexSeek"), "{store}");
        assert_eq!(q(seek), ["7"], "{store}");
        assert_eq!(q("MATCH (p:Place {at: point({x: -0.0, y: 0})}) RETURN p.i"), ["0"], "{store}");
        assert_eq!(
            q("MATCH (p:Place) WHERE point.distance(p.at, point({x: 0, y: 0})) < 10 RETURN p.i ORDER BY p.i"),
            ["0", "1", "2", "3", "4"],
            "{store}"
        );
        assert_eq!(
            q("MATCH (p:Place) WHERE point.withinBBox(p.at, point({x: 10, y: 0}), point({x: 12, y: 100})) RETURN p.i ORDER BY p.i"),
            ["10", "11", "12"],
            "{store}"
        );
        assert_eq!(q("MATCH (p:Place) WHERE p.at > point({x: 0, y: 0}) RETURN count(p)"), ["0"], "{store}");
        // Updates, then reopen again.
        run(&db, "MATCH (p:Place) WHERE p.i < 10 SET p.at = point({x: p.at.x, y: -1})");
        let db = reopen(db);
        assert_eq!(rows(&db, "MATCH (p:Place) WHERE p.at.y = -1 RETURN count(p)"), ["10"], "{store}");
        assert_eq!(rows(&db, "MATCH (p:Place {at: point({x: 5, y: -1})}) RETURN p.i"), ["5"], "{store}: the index follows updates");
        assert!(rows(&db, "MATCH (p:Place {at: point({x: 5, y: 10})}) RETURN p.i").is_empty(), "{store}");
    });
}
