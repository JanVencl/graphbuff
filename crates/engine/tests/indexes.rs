//! Composite and relationship property indexes (`CREATE INDEX … ON (n.a, n.b)`,
//! `CREATE INDEX … FOR ()-[r:T]-() ON (r.p)`), end to end on every store: seeks, plans,
//! index upkeep on writes and rollback, schema commands and durability.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Database, Error, QueryResult, StorageMode};

type Db = Database<BoxedStore>;

/// A fresh directory under the system temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-itest-{}-{}-{name}",
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

/// Runs `test` on a fresh database of every store kind. Its `reopen` closes the
/// database and opens it again from disk (the in-memory store is handed back as is).
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

fn setup(db: &Db, script: &str) {
    for stmt in graphbuff::cypher::split_statements(script).unwrap() {
        run(db, stmt);
    }
}

/// The rows of `q`, rendered and sorted (so plans may produce them in any order).
fn rows(db: &Db, q: &str) -> Vec<String> {
    let mut out: Vec<String> = run(db, q)
        .rows
        .iter()
        .map(|r| r.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect();
    out.sort();
    out
}

fn plan(db: &Db, q: &str) -> String {
    run(db, &format!("EXPLAIN {q}")).plan.expect("a plan").to_string()
}

/// Checks that `q` gives the same rows on `db` as on `plain` (same data, no indexes).
fn same(store: &str, db: &Db, plain: &Db, q: &str) {
    assert_eq!(rows(db, q), rows(plain, q), "{store}: results differ for `{q}`");
}

const PEOPLE: &str = "
    UNWIND range(0, 99) AS i
    CREATE (:P {id: i, a: i % 5, b: i % 7, c: CASE WHEN i % 2 = 0 THEN 'even' ELSE 'odd' END});
    CREATE (:P {id: 100, a: 1.0, b: 1});
    CREATE (:P {id: 101, a: 1});
    CREATE (:Q {id: 102, a: 1, b: 1});
";

#[test]
fn composite_node_indexes() {
    on_every_store(|store, db, reopen| {
        let plain = Database::open(StorageMode::InMemory).unwrap();
        setup(&db, PEOPLE);
        setup(&plain, PEOPLE);
        let r = run(&db, "CREATE INDEX p_ab FOR (p:P) ON (p.a, p.b)");
        assert_eq!(r.stats.indexes_added, 1);
        assert_eq!(
            rows(&db, "SHOW INDEXES"),
            ["\"p_ab\", \"NODE\", \"P\", [\"a\", \"b\"], \"RANGE\", {}"],
            "{store}"
        );
        // Order matters: (b, a) is another index; (a, b) again is a duplicate.
        assert!(matches!(
            db.execute("CREATE INDEX other FOR (p:P) ON (p.a, p.b)"),
            Err(Error::Constraint(_))
        ));
        assert_eq!(run(&db, "CREATE INDEX IF NOT EXISTS FOR (p:P) ON (p.a, p.b)").stats.indexes_added, 0);
        assert!(matches!(
            db.execute("CREATE INDEX FOR (p:P) ON (p.a, p.a)"),
            Err(Error::Semantic(_))
        ));

        let q = "MATCH (p:P {a: 1, b: 1}) RETURN p.id";
        assert!(plan(&db, q).contains("NodeIndexSeek (p:P {…}) on :P(a, b) ="), "{store}: {}", plan(&db, q));
        assert_eq!(rows(&db, q), ["1", "100", "36", "71"], "{store}");
        let queries = [
            q,
            "MATCH (p:P) WHERE p.a = 2 AND p.b = 3 RETURN p.id",
            "MATCH (p:P) WHERE p.b = 3 AND 2 = p.a AND p.c = 'odd' RETURN p.id",
            "MATCH (p:P {a: 1}) WHERE p.b = null RETURN p.id",
            "MATCH (p:P {a: 1, b: 1.0}) RETURN p.id",
            "MATCH (p:P {a: 9, b: 9}) RETURN p.id",
            "MATCH (p:P) WHERE p.a = 1 AND p.b IN [1, 2] RETURN p.id",
            "UNWIND [[1, 1], [2, 2], [4, 6]] AS k MATCH (p:P {a: k[0], b: k[1]}) RETURN k, p.id",
            "MATCH (x:P {id: 3}), (p:P {a: x.a, b: x.b}) RETURN p.id",
        ];
        for q in queries {
            same(store, &db, &plain, q);
        }

        // Writes keep the index in step: changed values, removed labels, deletes, and a
        // query that fails (rolled back).
        let writes = [
            "MATCH (p:P {id: 1}) SET p.b = 2",
            "MATCH (p:P {id: 36}) REMOVE p.a",
            "MATCH (p:P {id: 71}) REMOVE p:P",
            "MATCH (q:Q) SET q:P",
            "MATCH (p:P {id: 100}) DELETE p",
            "CREATE (:P {id: 200, a: 1, b: 1})",
        ];
        for w in writes {
            run(&db, w);
            run(&plain, w);
        }
        assert!(db.execute("MATCH (p:P {id: 2}) SET p.a = 1, p.b = 1 WITH p RETURN 1 / 0").is_err());
        let db = reopen(db);
        for q in queries {
            same(store, &db, &plain, q);
        }
        assert_eq!(rows(&db, "MATCH (p:P {a: 1, b: 1}) RETURN p.id"), ["102", "200"], "{store}");

        // The legacy spelling, and dropping it again.
        run(&db, "DROP INDEX ON :P(a, b)");
        assert!(rows(&db, "SHOW INDEXES").is_empty(), "{store}");
        run(&db, "CREATE INDEX ON :P(b, a)");
        assert_eq!(
            rows(&db, "SHOW INDEXES"),
            ["\"index_P_b_a\", \"NODE\", \"P\", [\"b\", \"a\"], \"RANGE\", {}"],
            "{store}"
        );
    });
}

#[test]
fn composite_indexes_on_long_values() {
    on_every_store(|store, db, reopen| {
        let long = |c: char| c.to_string().repeat(3000);
        let (x, y) = (long('x'), long('y'));
        setup(
            &db,
            &format!(
                "CREATE (:L {{id: 1, s: '{x}', t: '{y}'}});
                 CREATE (:L {{id: 2, s: '{x}', t: '{y}z'}});
                 CREATE (:L {{id: 3, s: '{x}z', t: '{y}'}});
                 CREATE INDEX FOR (n:L) ON (n.s, n.t)"
            ),
        );
        let db = reopen(db);
        let q = format!("MATCH (n:L {{s: '{x}', t: '{y}'}}) RETURN n.id");
        assert!(plan(&db, &q).contains(":L(s, t) ="), "{store}");
        assert_eq!(rows(&db, &q), ["1"], "{store}");
    });
}

const ROADS: &str = "
    UNWIND range(0, 29) AS i CREATE (:City {id: i});
    MATCH (a:City), (b:City) WHERE b.id = (a.id + 1) % 30
    CREATE (a)-[:ROAD {km: a.id * 10, name: 'r' + toString(a.id), lanes: a.id % 3}]->(b);
    MATCH (a:City {id: 4}) CREATE (a)-[:ROAD {km: 40, name: 'loop', lanes: 1}]->(a);
    MATCH (a:City {id: 7}), (b:City {id: 3}) CREATE (a)-[:RAIL {km: 70}]->(b);
";

#[test]
fn relationship_indexes() {
    on_every_store(|store, db, reopen| {
        let plain = Database::open(StorageMode::InMemory).unwrap();
        setup(&db, ROADS);
        setup(&plain, ROADS);
        run(&db, "CREATE INDEX road_km FOR ()-[r:ROAD]-() ON (r.km)");
        run(&db, "CREATE INDEX road_name IF NOT EXISTS FOR ()-[r:ROAD]->() ON (r.name)");
        assert_eq!(
            rows(&db, "SHOW INDEXES"),
            [
                "\"road_km\", \"RELATIONSHIP\", \"ROAD\", [\"km\"], \"RANGE\", {}",
                "\"road_name\", \"RELATIONSHIP\", \"ROAD\", [\"name\"], \"RANGE\", {}",
            ],
            "{store}"
        );
        assert!(matches!(
            db.execute("CREATE INDEX again FOR ()<-[r:ROAD]-() ON (r.km)"),
            Err(Error::Constraint(_))
        ));

        let q = "MATCH (a)-[r:ROAD {km: 50}]->(b) RETURN a.id, b.id";
        assert!(
            plan(&db, q).contains("DirectedRelationshipIndexSeek (a)-[r:ROAD {…}]->(b) on [:ROAD](km) ="),
            "{store}: {}",
            plan(&db, q)
        );
        assert_eq!(rows(&db, q), ["5, 6"], "{store}");
        let q2 = "MATCH (a)-[r:ROAD]-(b) WHERE r.km = 50 RETURN a.id, b.id";
        assert!(plan(&db, q2).contains("UndirectedRelationshipIndexSeek"), "{store}");
        assert_eq!(rows(&db, q2), ["5, 6", "6, 5"], "{store}");

        let queries = [
            q,
            q2,
            "MATCH (b)<-[r:ROAD {km: 50}]-(a) RETURN a.id, b.id",
            "MATCH (a)-[r:ROAD]-(b) WHERE r.km = 40 RETURN a.id, b.id",
            "MATCH (a)-[:ROAD {km: 120}]->(b:City {id: 13}) RETURN a.id",
            "MATCH (a)-[:ROAD {km: 120}]->(b:City {id: 14}) RETURN a.id",
            "MATCH (a)-[r:ROAD]->(b) WHERE r.km > 250 RETURN r.km",
            "MATCH (a)-[r:ROAD]->(b) WHERE r.km >= 100 AND r.km < 130 RETURN r.km",
            "MATCH (a)-[r:ROAD]->(b) WHERE r.km IN [10, 20, 10, null] RETURN r.km",
            "MATCH (a)-[r:ROAD]->(b) WHERE r.name STARTS WITH 'r2' RETURN r.name",
            "MATCH (a)-[r:ROAD]->(b) WHERE r.km = null RETURN r.km",
            "MATCH (a)-[r:ROAD {km: 70}]->(b) RETURN a.id",
            "MATCH (a)-[r:RAIL {km: 70}]->(b) RETURN a.id",
            // Longer patterns expand from both ends of the found relationship.
            "MATCH (x)-[:ROAD]->(a)-[r:ROAD {km: 100}]->(b)-[:ROAD]->(y) RETURN x.id, a.id, b.id, y.id",
            "MATCH p = (x)<-[:ROAD*1..2]-(a)-[r:ROAD {km: 100}]->(b) RETURN length(p), x.id",
            "MATCH (a)-[r:ROAD {km: 100}]->(b)-[s:ROAD]->(c) WHERE s.km = 110 RETURN c.id",
            // Relationship uniqueness within the MATCH.
            "MATCH (a)-[r:ROAD {km: 40}]-(b)-[s:ROAD]-(c) RETURN a.id, b.id, c.id",
            "MATCH (a)-[r:ROAD {km: 40}]->(a) RETURN a.id",
            // Bound variables and parameters from earlier clauses.
            "UNWIND [30, 60, 999] AS k MATCH (a)-[r:ROAD {km: k}]->(b) RETURN k, b.id",
            "MATCH (c:City {id: 2}) MATCH (a)-[r:ROAD]->(b) WHERE r.km = c.id * 10 RETURN a.id",
            "MATCH (a:City {id: 2}), (x)-[r:ROAD {km: 20}]->(y) RETURN x.id, y.id",
            "OPTIONAL MATCH (a)-[r:ROAD {km: 5}]->(b) RETURN a, r",
        ];
        for q in queries {
            same(store, &db, &plain, q);
        }

        // Upkeep on writes: property changes, deletes (direct and detached), rollback.
        let writes = [
            "MATCH ()-[r:ROAD {km: 50}]->() SET r.km = 55",
            "MATCH ()-[r:ROAD {km: 60}]->() REMOVE r.km",
            "MATCH ()-[r:ROAD {km: 70}]->() DELETE r",
            "MATCH (c:City {id: 9}) DETACH DELETE c",
            "MATCH (a:City {id: 0}), (b:City {id: 15}) CREATE (a)-[:ROAD {km: 50}]->(b)",
        ];
        for w in writes {
            run(&db, w);
            run(&plain, w);
        }
        assert!(db.execute("MATCH ()-[r:ROAD {km: 100}]->() SET r.km = 50 WITH r RETURN 1 / 0").is_err());
        let db = reopen(db);
        for q in queries {
            same(store, &db, &plain, q);
        }
        let moved = "MATCH (a)-[r:ROAD]->(b) WHERE r.km IN [50, 55, 60, 70, 80, 90] RETURN a.id, r.km, b.id";
        same(store, &db, &plain, moved);
        assert_eq!(rows(&db, "MATCH (a)-[r:ROAD {km: 50}]->(b) RETURN a.id, b.id"), ["0, 15"], "{store}");

        run(&db, "DROP INDEX road_km");
        assert!(!plan(&db, q).contains("RelationshipIndexSeek (a)-[r:ROAD {…}]->(b) on [:ROAD](km)"), "{store}");
        same(store, &db, &plain, q);
    });
}

#[test]
fn composite_relationship_indexes() {
    on_every_store(|store, db, reopen| {
        let plain = Database::open(StorageMode::InMemory).unwrap();
        setup(&db, ROADS);
        setup(&plain, ROADS);
        run(&db, "CREATE INDEX FOR ()-[r:ROAD]-() ON (r.lanes, r.km)");
        let db = reopen(db);
        let q = "MATCH (a)-[r:ROAD]->(b) WHERE r.lanes = 1 AND r.km = 40 RETURN a.id, b.id";
        assert!(plan(&db, q).contains("on [:ROAD](lanes, km) ="), "{store}: {}", plan(&db, q));
        assert_eq!(rows(&db, q), ["4, 4", "4, 5"], "{store}");
        for q in [
            q,
            "MATCH (a)-[r:ROAD {lanes: 2, km: 50}]-(b) RETURN a.id, b.id",
            "MATCH (a)-[r:ROAD {lanes: 0}]->(b) RETURN count(*)",
        ] {
            same(store, &db, &plain, q);
        }
    });
}
