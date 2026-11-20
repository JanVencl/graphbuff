//! Constraints (`CREATE CONSTRAINT … IS UNIQUE | IS NOT NULL | IS NODE KEY |
//! IS RELATIONSHIP KEY | IS :: <type>`) on nodes and relationships, end to end on every
//! store: enforcement, rollback, schema commands and durability.

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
            "graphbuff-ctest-{}-{}-{name}",
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

fn rows(db: &Db, q: &str) -> Vec<String> {
    run(db, q)
        .rows
        .iter()
        .map(|r| r.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect()
}

fn scalar(db: &Db, q: &str) -> String {
    let r = rows(db, q);
    assert_eq!(r.len(), 1, "`{q}`: {r:?}");
    r[0].clone()
}

/// `q` fails with a constraint error mentioning `needle`, and changes nothing.
fn violates(store: &str, db: &Db, q: &str, needle: &str) {
    let before = rows(db, "MATCH (n) RETURN n ORDER BY id(n)");
    match db.execute(q) {
        Err(Error::Constraint(msg)) => assert!(msg.contains(needle), "{store}: `{q}`: {msg}"),
        other => panic!("{store}: `{q}` should violate a constraint, got {other:?}"),
    }
    assert_eq!(rows(db, "MATCH (n) RETURN n ORDER BY id(n)"), before, "{store}: `{q}` was not rolled back");
}

#[test]
fn unique_constraints() {
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE (:P {id: 1}), (:P {id: 2}), (:P), (:Q {id: 1})");
        let r = run(&db, "CREATE CONSTRAINT p_id FOR (p:P) REQUIRE p.id IS UNIQUE");
        assert_eq!((r.stats.constraints_added, r.stats.indexes_added), (1, 1), "{store}");
        assert_eq!(rows(&db, "SHOW CONSTRAINTS"), ["\"p_id\", \"UNIQUE\", \"P\", [\"id\"], \"NODE\", null"]);
        assert_eq!(rows(&db, "SHOW INDEXES"), ["\"p_id\", \"NODE\", \"P\", [\"id\"], \"RANGE\", {}"], "{store}: backing index");

        violates(store, &db, "CREATE (:P {id: 1})", "both have id = 1");
        violates(store, &db, "CREATE (:P {id: 1.0})", "constraint `p_id`"); // 1 = 1.0
        violates(store, &db, "UNWIND [3, 3] AS i CREATE (:P {id: i})", "id = 3");
        violates(store, &db, "MATCH (p:P {id: 2}) SET p.id = 1", "id = 1");
        violates(store, &db, "MATCH (q:Q) SET q:P", "id = 1");
        violates(store, &db, "MATCH (p:P) WHERE p.id IS NULL SET p.id = 2", "id = 2");
        violates(store, &db, "MATCH (p:P {id: 2}) SET p += {id: 1}", "id = 1");

        // Allowed: other labels, missing values, other values, and a query that passes
        // through a duplicate but ends without one.
        run(&db, "CREATE (:Q {id: 1}), (:P), (:P {id: 3})");
        run(&db, "MATCH (a:P {id: 1}), (b:P {id: 2}) SET a.id = 2, b.id = 1");
        assert_eq!(rows(&db, "MATCH (p:P) WHERE p.id IS NOT NULL RETURN p.id ORDER BY p.id"), ["1", "2", "3"]);
        // MERGE finds the existing node instead of creating a duplicate.
        assert_eq!(run(&db, "MERGE (p:P {id: 3}) RETURN p.id").stats.nodes_created, 0);
        // Deleted nodes free their value.
        run(&db, "MATCH (p:P {id: 3}) DELETE p CREATE (:P {id: 3})");

        let db = reopen(db);
        violates(store, &db, "CREATE (:P {id: 3})", "id = 3");
        assert_eq!(scalar(&db, "MATCH (p:P) RETURN count(p)"), "5", "{store}");
    });
}

#[test]
fn not_null_constraints() {
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE (:P {name: 'a'}), (:Q)");
        run(&db, "CREATE CONSTRAINT FOR (p:P) REQUIRE p.name IS NOT NULL");
        assert_eq!(rows(&db, "SHOW CONSTRAINTS"), ["\"constraint_P_name\", \"NOT NULL\", \"P\", [\"name\"], \"NODE\", null"]);
        assert_eq!(rows(&db, "SHOW INDEXES"), Vec::<String>::new(), "{store}: no index needed");
        violates(store, &db, "CREATE (:P)", "must have the property `name`");
        violates(store, &db, "CREATE (:P {name: null})", "`name`");
        violates(store, &db, "MATCH (p:P) REMOVE p.name", "`name`");
        violates(store, &db, "MATCH (p:P) SET p.name = null", "`name`");
        violates(store, &db, "MATCH (p:P) SET p = {other: 1}", "`name`");
        violates(store, &db, "MATCH (q:Q) SET q:P", "`name`");
        // Set later in the same query is fine; so is dropping the label with the value.
        run(&db, "CREATE (p:P) SET p.name = 'b'");
        run(&db, "MATCH (p:P {name: 'b'}) REMOVE p:P, p.name");
        run(&db, "CREATE (:P {name: 'a'})"); // not unique, so duplicates are fine
        let db = reopen(db);
        violates(store, &db, "CREATE (:P {x: 1})", "`name`");
    });
}

#[test]
fn node_key_constraints() {
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE (:P {a: 1, b: 1}), (:P {a: 1, b: 2})");
        run(&db, "CREATE CONSTRAINT key FOR (p:P) REQUIRE (p.a, p.b) IS NODE KEY");
        violates(store, &db, "CREATE (:P {a: 1, b: 1})", "a = 1, b = 1");
        violates(store, &db, "CREATE (:P {a: 2})", "must have the property `b`");
        violates(store, &db, "CREATE (:P {b: 2})", "must have the property `a`");
        violates(store, &db, "MATCH (p:P {b: 2}) SET p.b = 1", "a = 1, b = 1");
        run(&db, "CREATE (:P {a: 2, b: 1}), (:P {a: 1, b: 3})");
        let db = reopen(db);
        violates(store, &db, "CREATE (:P {a: 2, b: 1})", "constraint `key`");
        assert_eq!(scalar(&db, "MATCH (p:P) RETURN count(p)"), "4");
    });
}

#[test]
fn existing_data_must_satisfy_a_new_constraint() {
    on_every_store(|store, db, _| {
        run(&db, "CREATE (:P {id: 1}), (:P {id: 1}), (:P)");
        for (q, needle) in [
            ("CREATE CONSTRAINT u FOR (p:P) REQUIRE p.id IS UNIQUE", "both have id = 1"),
            ("CREATE CONSTRAINT n FOR (p:P) REQUIRE p.id IS NOT NULL", "must have the property `id`"),
            ("CREATE CONSTRAINT k FOR (p:P) REQUIRE (p.id) IS NODE KEY", "existing data violates it"),
        ] {
            match db.execute(q) {
                Err(Error::Constraint(msg)) => assert!(msg.contains(needle), "{store}: {msg}"),
                other => panic!("{store}: `{q}` should fail, got {other:?}"),
            }
        }
        // Nothing is left behind: no constraint, and no backing index.
        assert!(rows(&db, "SHOW CONSTRAINTS").is_empty(), "{store}");
        assert!(rows(&db, "SHOW INDEXES").is_empty(), "{store}");
        run(&db, "CREATE (:P {id: 1})");
    });
}

#[test]
fn schema_commands() {
    on_every_store(|store, db, reopen| {
        // A constraint reuses an existing index, and leaves it when dropped.
        run(&db, "CREATE INDEX by_id FOR (p:P) ON (p.id)");
        let r = run(&db, "CREATE CONSTRAINT u FOR (p:P) REQUIRE p.id IS UNIQUE");
        assert_eq!((r.stats.constraints_added, r.stats.indexes_added), (1, 0));
        // The index a constraint seeks on cannot be dropped under it.
        match db.execute("DROP INDEX by_id") {
            Err(Error::Constraint(msg)) => assert!(msg.contains("used by constraint `u`"), "{msg}"),
            other => panic!("{store}: {other:?}"),
        }
        // Names and equivalent definitions clash; IF NOT EXISTS makes that a no-op.
        assert!(db.execute("CREATE CONSTRAINT u FOR (p:P) REQUIRE p.x IS UNIQUE").is_err());
        assert!(db.execute("CREATE CONSTRAINT v FOR (p:P) REQUIRE p.id IS UNIQUE").is_err());
        let r = run(&db, "CREATE CONSTRAINT v IF NOT EXISTS FOR (p:P) REQUIRE p.id IS UNIQUE");
        assert!(!r.stats.contains_updates());
        assert!(db.execute("CREATE CONSTRAINT FOR (p:P) REQUIRE (p.a, p.b) IS NOT NULL").is_err());
        assert!(db.execute("CREATE CONSTRAINT FOR (p:P) REQUIRE (p.a, p.a) IS UNIQUE").is_err());
        // A constraint's own index goes with it.
        run(&db, "CREATE CONSTRAINT w FOR (p:Q) REQUIRE p.id IS UNIQUE");
        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW CONSTRAINTS").len(), 2, "{store}");
        let r = run(&db, "DROP CONSTRAINT w");
        assert_eq!((r.stats.constraints_removed, r.stats.indexes_removed), (1, 1));
        let r = run(&db, "DROP CONSTRAINT u");
        assert_eq!((r.stats.constraints_removed, r.stats.indexes_removed), (1, 0));
        assert_eq!(rows(&db, "SHOW INDEXES"), ["\"by_id\", \"NODE\", \"P\", [\"id\"], \"RANGE\", {}"]);
        run(&db, "DROP INDEX by_id");
        assert!(db.execute("DROP CONSTRAINT u").is_err());
        assert!(!run(&db, "DROP CONSTRAINT u IF EXISTS").stats.contains_updates());
        run(&db, "CREATE (:P {id: 1}), (:P {id: 1})"); // no longer enforced
        let db = reopen(db);
        assert!(rows(&db, "SHOW CONSTRAINTS").is_empty(), "{store}");
    });
}

#[test]
fn explain_and_profile_with_constraints() {
    on_every_store(|store, db, _| {
        run(&db, "CREATE CONSTRAINT u FOR (p:P) REQUIRE p.id IS UNIQUE");
        // EXPLAIN never runs the query, so it never violates anything.
        run(&db, "CREATE (:P {id: 1})");
        assert!(run(&db, "EXPLAIN CREATE (:P {id: 1})").plan.is_some());
        violates(store, &db, "PROFILE CREATE (:P {id: 1})", "id = 1");
    });
}

/// `q` fails with a constraint error mentioning `needle`, and leaves the relationships
/// as they were.
fn violates_rels(store: &str, db: &Db, q: &str, needle: &str) {
    let before = rows(db, "MATCH ()-[r]->() RETURN r, properties(r) ORDER BY id(r)");
    violates(store, db, q, needle);
    assert_eq!(rows(db, "MATCH ()-[r]->() RETURN r, properties(r) ORDER BY id(r)"), before, "{store}: `{q}`");
}

#[test]
fn relationship_constraints() {
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE (a:A), (b:B), (a)-[:R {id: 1, w: 1}]->(b), (a)-[:R {id: 2, w: 1}]->(b), (a)-[:S {id: 1}]->(b)");
        let r = run(&db, "CREATE CONSTRAINT r_id FOR ()-[r:R]-() REQUIRE r.id IS UNIQUE");
        assert_eq!((r.stats.constraints_added, r.stats.indexes_added), (1, 1), "{store}");
        assert_eq!(rows(&db, "SHOW CONSTRAINTS"), ["\"r_id\", \"UNIQUE\", \"R\", [\"id\"], \"RELATIONSHIP\", null"]);
        assert_eq!(
            rows(&db, "SHOW INDEXES"),
            ["\"r_id\", \"RELATIONSHIP\", \"R\", [\"id\"], \"RANGE\", {}"],
            "{store}: backing index"
        );
        violates_rels(store, &db, "MATCH (a:A), (b:B) CREATE (a)-[:R {id: 1}]->(b)", "relationships");
        violates_rels(store, &db, "MATCH ()-[r:R {id: 2}]->() SET r.id = 1", "of type `R` both have id = 1");
        // Other types and missing values are fine; so is a swap.
        run(&db, "MATCH (a:A), (b:B) CREATE (a)-[:S {id: 2}]->(b), (a)-[:R {w: 1}]->(b)");
        run(&db, "MATCH ()-[x:R {id: 1}]->(), ()-[y:R {id: 2}]->() SET x.id = 2, y.id = 1");

        run(&db, "CREATE CONSTRAINT r_w FOR ()-[r:R]-() REQUIRE r.w IS NOT NULL");
        violates_rels(store, &db, "MATCH (a:A), (b:B) CREATE (a)-[:R {id: 9}]->(b)", "must have the property `w`");
        violates_rels(store, &db, "MATCH ()-[r:R {id: 1}]->() REMOVE r.w", "relationship");
        assert!(
            matches!(db.execute("CREATE CONSTRAINT FOR ()-[r:S]-() REQUIRE r.w IS NOT NULL"), Err(Error::Constraint(m)) if m.contains("existing data")),
            "{store}"
        );

        run(&db, "MATCH (a:A), (b:B) CREATE (a)-[:K {a: 1, b: 1}]->(b)");
        run(&db, "CREATE CONSTRAINT k FOR ()-[k:K]-() REQUIRE (k.a, k.b) IS RELATIONSHIP KEY");
        violates_rels(store, &db, "MATCH (a:A), (b:B) CREATE (a)-[:K {a: 1}]->(b)", "`b`");
        violates_rels(store, &db, "MATCH (a:A), (b:B) CREATE (b)-[:K {a: 1, b: 1}]->(a)", "a = 1, b = 1");
        run(&db, "MATCH (a:A), (b:B) CREATE (a)-[:K {a: 1, b: 2}]->(b)");

        // A deleted relationship frees its value.
        run(&db, "MATCH ()-[r:R {id: 1}]->() DELETE r WITH 1 AS x MATCH (a:A), (b:B) CREATE (a)-[:R {id: 1, w: 0}]->(b)");

        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW CONSTRAINTS").len(), 3, "{store}");
        violates_rels(store, &db, "MATCH (a:A), (b:B) CREATE (a)-[:R {id: 2, w: 0}]->(b)", "id = 2");
        violates_rels(store, &db, "MATCH (a:A), (b:B) CREATE (a)-[:K {a: 1, b: 2}]->(b)", "constraint `k`");
        // Node constraints of the same name as the type do not apply to relationships.
        run(&db, "CREATE (:R {id: 1}), (:R {id: 1})");
        run(&db, "DROP CONSTRAINT r_id");
        assert_eq!(scalar(&db, "CALL db.indexes() YIELD name RETURN collect(name)"), "[\"k\"]", "{store}: the backing index goes with it");
        run(&db, "MATCH (a:A), (b:B) CREATE (a)-[:R {id: 2, w: 0}]->(b)");
        if store == "persistent" {
            db.check().unwrap_or_else(|e| panic!("{store}: {e}"));
        }
    });
}

#[test]
fn property_type_constraints() {
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE (:P {n: 1}), (:P), (:Q {n: 'x'})");
        run(&db, "CREATE CONSTRAINT p_n FOR (p:P) REQUIRE p.n IS :: INTEGER | FLOAT");
        run(&db, "CREATE CONSTRAINT p_tags FOR (p:P) REQUIRE p.tags IS TYPED LIST<STRING NOT NULL>");
        run(&db, "CREATE CONSTRAINT r_at FOR ()-[r:R]-() REQUIRE r.at :: ZONED DATETIME");
        assert_eq!(
            rows(&db, "CALL db.constraints() YIELD name, type, propertyType RETURN name, type, propertyType ORDER BY name"),
            [
                "\"p_n\", \"PROPERTY TYPE\", \"INTEGER | FLOAT\"",
                "\"p_tags\", \"PROPERTY TYPE\", \"LIST<STRING NOT NULL>\"",
                "\"r_at\", \"PROPERTY TYPE\", \"ZONED DATETIME\"",
            ],
            "{store}"
        );
        violates(store, &db, "CREATE (:P {n: '1'})", "has `n` of type STRING, but constraint `p_n` requires INTEGER | FLOAT");
        violates(store, &db, "MATCH (p:P) SET p.n = true", "BOOLEAN");
        violates(store, &db, "MATCH (q:Q) SET q:P", "`p_n`");
        violates(store, &db, "CREATE (:P {tags: [1]})", "LIST<STRING NOT NULL>");
        violates(store, &db, "CREATE (:P {tags: ['a', 1]})", "`p_tags`");
        violates_rels(store, &db, "CREATE ()-[:R {at: date()}]->()", "relationship");
        violates_rels(store, &db, "CREATE ()-[:R {at: localdatetime()}]->()", "LOCAL DATETIME");
        // Missing, matching and other labels' values are fine.
        run(&db, "CREATE (:P), (:P {n: 2.5, tags: []}), (:P {tags: ['a', 'b']}), (:Q {n: true})");
        run(&db, "CREATE ()-[:R {at: datetime()}]->(), ()-[:R]->(), ()-[:S {at: 1}]->()");

        // One type per property: another type is a clash, the same one is a no-op with IF NOT EXISTS.
        assert!(
            matches!(db.execute("CREATE CONSTRAINT FOR (p:P) REQUIRE p.n IS :: STRING"), Err(Error::Constraint(m)) if m.contains("`p_n`")),
            "{store}"
        );
        run(&db, "CREATE CONSTRAINT IF NOT EXISTS FOR (p:P) REQUIRE p.n IS :: INTEGER | FLOAT");
        assert!(
            matches!(db.execute("CREATE CONSTRAINT FOR (q:Q) REQUIRE q.n IS :: STRING"), Err(Error::Constraint(m)) if m.contains("existing data")),
            "{store}"
        );

        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW CONSTRAINTS").len(), 3, "{store}");
        violates(store, &db, "CREATE (:P {n: 'x'})", "`p_n`");
        violates_rels(store, &db, "CREATE ()-[:R {at: 1}]->()", "`r_at`");
        if store == "persistent" {
            db.check().unwrap_or_else(|e| panic!("{store}: {e}"));
        }
    });
}
