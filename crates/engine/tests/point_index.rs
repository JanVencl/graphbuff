//! Point indexes, end to end: `CREATE POINT INDEX` on every store, index seeks for
//! `point.distance(…) < r` and `point.withinBBox(…)` on nodes and relationships (the same
//! rows as without the index), updates, and transactions.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Database, QueryResult, StorageMode};

type Db = Database<BoxedStore>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-pointtest-{}-{}-{name}",
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

fn mem() -> Db {
    Database::open(StorageMode::InMemory).unwrap()
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

/// Places on a 40 × 40 grid of cartesian points one apart, and as many WGS-84 ones
/// spread over the globe (with a few on the 180th meridian and near the poles).
fn places(db: &Db) {
    run(
        db,
        "UNWIND range(0, 1599) AS i
         CREATE (:Place {k: i, at: point({x: i % 40, y: i / 40})})",
    );
    run(
        db,
        "UNWIND range(0, 1599) AS i
         CREATE (:City {k: i, at: point({longitude: (i * 37 % 361) - 180.0, latitude: (i * 13 % 179) - 89.0})})",
    );
    // Not indexed: other labels, no property.
    run(db, "CREATE (:Other {at: point({x: 1, y: 1})}), (:Place {k: -2})");
}

const QUERIES: [&str; 10] = [
    "MATCH (p:Place) WHERE point.distance(p.at, point({x: 10, y: 10})) < 3 RETURN p.k ORDER BY p.k",
    "MATCH (p:Place) WHERE point.distance(point({x: 10, y: 10}), p.at) <= 3 RETURN p.k ORDER BY p.k",
    "MATCH (p:Place) WHERE 2.5 > point.distance(p.at, point({x: 0, y: 39})) RETURN p.k ORDER BY p.k",
    "MATCH (p:Place) WHERE point.withinBBox(p.at, point({x: 5, y: 5}), point({x: 7, y: 6})) RETURN p.k ORDER BY p.k",
    "MATCH (p:Place) WHERE point.withinBBox(p.at, point({x: 7, y: 5}), point({x: 5, y: 6})) RETURN p.k ORDER BY p.k",
    "MATCH (p:Place) WHERE point.distance(p.at, point({longitude: 1, latitude: 1})) < 10 RETURN p.k",
    "MATCH (c:City) WHERE point.distance(c.at, point({longitude: 179.5, latitude: 0})) < 2000000 RETURN c.k ORDER BY c.k",
    "MATCH (c:City) WHERE point.distance(c.at, point({longitude: 0, latitude: 89.5})) < 1500000 RETURN c.k ORDER BY c.k",
    "MATCH (c:City) WHERE point.withinBBox(c.at, point({longitude: 170, latitude: -30}), point({longitude: -170, latitude: 30})) RETURN c.k ORDER BY c.k",
    "MATCH (c:City) WHERE point.distance(c.at, null) < 10 OR c.k = 3 RETURN c.k",
];

#[test]
fn seeks_find_the_same_rows_as_scans() {
    let plain = mem();
    places(&plain);
    let want: Vec<Vec<String>> = QUERIES.iter().map(|q| rows(&plain, q)).collect();
    assert!(want.iter().filter(|r| !r.is_empty()).count() >= 7, "{want:?}");
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE POINT INDEX places FOR (p:Place) ON (p.at)");
        places(&db);
        run(&db, "CREATE POINT INDEX cities FOR (c:City) ON (c.at)");
        let db = reopen(db);
        for (q, want) in QUERIES.iter().zip(&want) {
            assert_eq!(&rows(&db, q), want, "{store}: {q}");
        }
        assert_eq!(db.cached_indexes(), 2, "{store}: the seeks read both indexes");
    });
}

#[test]
fn explain_shows_the_seek() {
    let db = mem();
    places(&db);
    let q = "MATCH (p:Place) WHERE point.distance(p.at, $c) < 3 RETURN p";
    assert!(!db.explain(q).unwrap().contains("NodeIndexSeek"));
    run(&db, "CREATE POINT INDEX FOR (p:Place) ON (p.at)");
    let plan = db.explain(q).unwrap();
    assert!(plan.contains("NodeIndexSeek (p:Place) on :Place(at) point.distance <"), "{plan}");
    let plan = db
        .explain("MATCH (p:Place) WHERE point.withinBBox(p.at, $lo, $hi) RETURN p")
        .unwrap();
    assert!(plan.contains("on :Place(at) point.withinBBox"), "{plan}");
    // A range index does not answer distance seeks, nor a point index equality ones.
    run(&db, "CREATE INDEX FOR (c:City) ON (c.at)");
    let plan = db.explain("MATCH (c:City) WHERE point.distance(c.at, $c) < 3 RETURN c").unwrap();
    assert!(!plan.contains("NodeIndexSeek"), "{plan}");
    let plan = db.explain("MATCH (p:Place) WHERE p.at = $c RETURN p").unwrap();
    assert!(!plan.contains("NodeIndexSeek"), "{plan}");
}

#[test]
fn definitions_options_and_errors() {
    on_every_store(|store, db, reopen| {
        let r = run(&db, "CREATE POINT INDEX places FOR (p:Place) ON (p.at) OPTIONS {indexConfig: {`spatial.cartesian.min`: [-100.0, -100.0], `spatial.cartesian.max`: [100.0, 100.0]}}");
        assert_eq!(r.stats.indexes_added, 1);
        run(&db, "CREATE POINT INDEX FOR ()-[r:ROUTE]-() ON (r.via)");
        run(&db, "CREATE INDEX FOR (p:Place) ON (p.at)");
        let db = reopen(db);
        let want = [
            "\"index_Place_at\", \"NODE\", \"Place\", [\"at\"], \"RANGE\", {}",
            "\"places\", \"NODE\", \"Place\", [\"at\"], \"POINT\", {indexConfig: {}}",
            "\"point_ROUTE_via\", \"RELATIONSHIP\", \"ROUTE\", [\"via\"], \"POINT\", {indexConfig: {}}",
        ];
        assert_eq!(rows(&db, "SHOW INDEXES"), want, "{store}");
        let err = db.execute("CREATE POINT INDEX other FOR (p:Place) ON (p.at)").unwrap_err().to_string();
        assert!(err.contains("a point index on :Place(at) already exists"), "{store}: {err}");
        assert_eq!(run(&db, "CREATE POINT INDEX other IF NOT EXISTS FOR (p:Place) ON (p.at)").stats.indexes_added, 0);
        for bad in [
            "CREATE POINT INDEX FOR (p:Place) ON (p.a, p.b)",
            "CREATE POINT INDEX FOR (p:Place) ON (p.b) OPTIONS {indexConfig: {`spatial.cartesian.min`: [1]}}",
            "CREATE POINT INDEX FOR (p:Place) ON (p.b) OPTIONS {indexConfig: {`vector.dimensions`: 3}}",
        ] {
            assert!(db.execute(bad).is_err(), "{store}: {bad}");
        }
        assert_eq!(run(&db, "DROP INDEX places").stats.indexes_removed, 1);
        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 2, "{store}");
    });
}

#[test]
fn relationship_seeks() {
    let db = mem();
    run(&db, "CREATE POINT INDEX FOR ()-[r:ROUTE]-() ON (r.via)");
    run(
        &db,
        "UNWIND range(0, 99) AS i
         CREATE (:Stop {k: i})-[:ROUTE {k: i, via: point({x: i, y: 0})}]->(:Stop {k: i + 100})",
    );
    let q = "MATCH (a)-[r:ROUTE]->(b) WHERE point.distance(r.via, point({x: 50, y: 0})) <= 2 RETURN r.k, a.k, b.k ORDER BY r.k";
    assert!(db.explain(q).unwrap().contains("RelationshipIndexSeek"), "{}", db.explain(q).unwrap());
    assert_eq!(rows(&db, q), ["48, 48, 148", "49, 49, 149", "50, 50, 150", "51, 51, 151", "52, 52, 152"]);
}

#[test]
fn commits_update_the_index() {
    let db = mem();
    run(&db, "CREATE POINT INDEX FOR (p:Place) ON (p.at)");
    places(&db);
    let near = "MATCH (p:Place) WHERE point.distance(p.at, point({x: 100, y: 100})) < 1 RETURN p.k ORDER BY p.k";
    assert!(rows(&db, near).is_empty());
    // Moved, added, relabelled and deleted after the index was read.
    run(&db, "MATCH (p:Place {k: 0}) SET p.at = point({x: 100, y: 100.5})");
    run(&db, "CREATE (:Place {k: 5000, at: point({x: 100.2, y: 99.9})})");
    run(&db, "MATCH (o:Other) SET o:Place, o.k = 6000, o.at = point({x: 99.5, y: 100})");
    assert_eq!(rows(&db, near), ["0", "5000", "6000"]);
    run(&db, "MATCH (p:Place {k: 5000}) DELETE p");
    run(&db, "MATCH (p:Place {k: 6000}) REMOVE p:Place");
    run(&db, "MATCH (p:Place {k: 0}) REMOVE p.at");
    assert!(rows(&db, near).is_empty());
    assert!(rows(&db, "MATCH (p:Place) WHERE point.distance(p.at, point({x: 0, y: 0})) < 0.5 RETURN p.k").is_empty());
}

#[test]
fn transactions_see_their_own_writes() {
    let db = mem();
    run(&db, "CREATE POINT INDEX FOR (p:Place) ON (p.at)");
    places(&db);
    let q = "MATCH (p:Place) WHERE point.distance(p.at, point({x: 100, y: 100})) < 1 RETURN p.k";
    assert!(rows(&db, q).is_empty());
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:Place {k: 7000, at: point({x: 100, y: 100})})").unwrap();
    let r = tx.execute(q).unwrap();
    assert_eq!(r.rows.len(), 1);
    // Not seen outside until committed.
    assert!(rows(&db, q).is_empty());
    tx.commit().unwrap();
    assert_eq!(rows(&db, q), ["7000"]);
}
