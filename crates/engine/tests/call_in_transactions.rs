//! `CALL { … } IN TRANSACTIONS`: batches of input rows, each committed on its own.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use graphbuff::{BoxedStore, Database, Error, QueryOptions, ResultValue, StorageMode};

type Db = Database<BoxedStore>;

/// A fresh directory under the system temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-cit-{}-{}-{name}",
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

/// Runs `test` on a database of every storage mode; the second argument reopens it.
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

fn int(db: &Db, q: &str) -> i64 {
    match &db.execute(q).unwrap().rows[0][0] {
        ResultValue::Int(i) => *i,
        other => panic!("{q}: expected an integer, got {other:?}"),
    }
}

/// The rows of `q`, each as its values' debug form joined by ", ".
fn rows(db: &Db, q: &str) -> Vec<String> {
    let r = db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    r.rows
        .iter()
        .map(|row| row.iter().map(|v| format!("{v}")).collect::<Vec<_>>().join(", "))
        .collect()
}

fn err(db: &Db, q: &str) -> Error {
    match db.execute(q) {
        Ok(r) => panic!("{q}: expected an error, got {:?}", r.rows),
        Err(e) => e,
    }
}

#[test]
fn creates_in_batches_on_every_store() {
    on_every_store(|store, db, reopen| {
        let r = db
            .execute("UNWIND range(1, 10) AS i CALL { WITH i CREATE (:N {i: i}) } IN TRANSACTIONS OF 3 ROWS")
            .unwrap();
        assert_eq!(r.stats.nodes_created, 10, "{store}");
        assert_eq!(r.stats.properties_set, 10, "{store}");
        assert!(r.columns.is_empty() && r.rows.is_empty(), "{store}");
        let db = reopen(db);
        assert_eq!(int(&db, "MATCH (n:N) RETURN sum(n.i)"), 55, "{store}");
    });
}

#[test]
fn returning_subqueries_and_clauses_after() {
    let db = mem();
    let q = "UNWIND range(1, 5) AS i
             CALL (i) { CREATE (n:M {i: i}) RETURN n.i * 10 AS x } IN TRANSACTIONS OF 2 ROWS
             RETURN i, x ORDER BY i";
    assert_eq!(rows(&db, q), ["1, 10", "2, 20", "3, 30", "4, 40", "5, 50"]);
    // Nodes from the batches can be returned.
    let q = "UNWIND [1, 2] AS i CALL { WITH i CREATE (n:R {i: i}) RETURN n } IN TRANSACTIONS RETURN n.i AS i ORDER BY i";
    assert_eq!(rows(&db, q), ["1", "2"]);
    // A unit subquery keeps each input row.
    let q = "MATCH (m:M) CALL { WITH m SET m.done = true } IN TRANSACTIONS OF 1 ROW RETURN count(*) AS n";
    let r = db.execute(q).unwrap();
    assert_eq!(r.rows[0][0], ResultValue::Int(5));
    assert_eq!(r.stats.properties_set, 5);
    assert_eq!(int(&db, "MATCH (m:M) WHERE m.done RETURN count(m)"), 5);
    // Without input clauses the subquery runs once.
    assert_eq!(db.execute("CALL { CREATE (:One) } IN TRANSACTIONS").unwrap().stats.nodes_created, 1);
}

#[test]
fn each_batch_sees_the_ones_before() {
    let db = mem();
    db.execute("CREATE (:Counter {n: 0})").unwrap();
    db.execute("UNWIND range(1, 7) AS i CALL { MATCH (c:Counter) SET c.n = c.n + 1 } IN TRANSACTIONS OF 2 ROWS")
        .unwrap();
    assert_eq!(int(&db, "MATCH (c:Counter) RETURN c.n"), 7);
}

#[test]
fn deletes_what_it_matched() {
    on_every_store(|store, db, reopen| {
        db.execute("UNWIND range(1, 2500) AS i CREATE (:D {i: i})-[:R]->(:E)").unwrap();
        let r = db.execute("MATCH (d:D) CALL (d) { DETACH DELETE d } IN TRANSACTIONS OF 1000 ROWS").unwrap();
        assert_eq!((r.stats.nodes_deleted, r.stats.relationships_deleted), (2500, 2500), "{store}");
        let db = reopen(db);
        assert_eq!(int(&db, "MATCH (d:D) RETURN count(d)"), 0, "{store}");
        assert_eq!(int(&db, "MATCH (e:E) RETURN count(e)"), 2500, "{store}");
    });
}

#[test]
fn a_failed_batch_keeps_the_ones_before() {
    let db = mem();
    // The third row divides by zero: batches of one row, so two commit first.
    let q = "UNWIND [1, 2, 0, 4] AS i CALL { WITH i CREATE (:F {v: 10 / i}) } IN TRANSACTIONS OF 1 ROW";
    err(&db, q);
    assert_eq!(int(&db, "MATCH (f:F) RETURN count(f)"), 2);
    // ON ERROR FAIL is the default.
    err(&db, &format!("{q} ON ERROR FAIL"));
    assert_eq!(int(&db, "MATCH (f:F) RETURN count(f)"), 4);
}

#[test]
fn on_error_continue_and_break_with_status() {
    let db = mem();
    let q = "UNWIND [1, 2, 0, 4] AS i
             CALL { WITH i CREATE (f:G {v: 12 / i}) RETURN f.v AS v } IN TRANSACTIONS OF 1 ROW
             ON ERROR CONTINUE REPORT STATUS AS s
             RETURN i, v, s.started, s.committed, s.errorMessage IS NULL AS ok, s.transactionId IS NULL AS noTx";
    assert_eq!(
        rows(&db, q),
        [
            "1, 12, true, true, true, false",
            "2, 6, true, true, true, false",
            "0, null, true, false, false, false",
            "4, 3, true, true, true, false",
        ]
    );
    assert_eq!(int(&db, "MATCH (g:G) RETURN count(g)"), 3);

    let q = "UNWIND [1, 0, 2, 3] AS i
             CALL { WITH i CREATE (:H {v: 12 / i}) } IN TRANSACTIONS OF 1 ROW
             ON ERROR BREAK REPORT STATUS AS s
             RETURN i, s.started, s.committed";
    assert_eq!(
        rows(&db, q),
        ["1, true, true", "0, true, false", "2, false, false", "3, false, false"]
    );
    assert_eq!(int(&db, "MATCH (h:H) RETURN count(h)"), 1);
    // CONTINUE without a status, and options in any order.
    let r = db
        .execute(
            "UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:K {v: 1 / i}) } IN TRANSACTIONS ON ERROR CONTINUE OF 1 ROW",
        )
        .unwrap();
    assert_eq!(r.stats.nodes_created, 2);
}

#[test]
fn concurrent_is_accepted() {
    let db = mem();
    let r = db
        .execute("UNWIND range(1, 6) AS i CALL { WITH i CREATE (:C) } IN 3 CONCURRENT TRANSACTIONS OF 2 ROWS")
        .unwrap();
    assert_eq!(r.stats.nodes_created, 6);
    let r = db.execute("UNWIND range(1, 2) AS i CALL { WITH i CREATE (:C) } IN CONCURRENT TRANSACTIONS").unwrap();
    assert_eq!(r.stats.nodes_created, 2);
}

#[test]
fn parameters_and_options() {
    let db = mem();
    let params = graphbuff::params! { "n" => 2, "rows" => vec![1, 2, 3] };
    let r = db
        .execute_with_params("UNWIND $rows AS i CALL { WITH i CREATE (:P {i: i}) } IN TRANSACTIONS OF $n ROWS", &params)
        .unwrap();
    assert_eq!(r.stats.nodes_created, 3);
    for (q, want) in [
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS OF 0 ROWS", "positive integer"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS OF 'a' ROWS", "positive integer"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS OF 2", "expected ROWS"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS ON ERROR FAIL REPORT STATUS AS s RETURN s", "REPORT STATUS"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS ON ERROR RETRY", "RETRY"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS OF 1 ROW OF 2 ROWS", "twice"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN 2 TRANSACTIONS", "CONCURRENT"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS REPORT STATUS AS i ON ERROR CONTINUE RETURN i", "already declared"),
    ] {
        let e = err(&db, q).to_string();
        assert!(e.contains(want), "{q}: {e}");
    }
    assert_eq!(int(&db, "MATCH (x:X) RETURN count(x)"), 0);
}

#[test]
fn where_it_may_appear() {
    let db = mem();
    for (q, want) in [
        ("CREATE (:A) WITH 1 AS x CALL { CREATE (:B) } IN TRANSACTIONS", "may only read"),
        ("UNWIND [1] AS i CALL { WITH i CREATE (:B) } IN TRANSACTIONS CREATE (:C)", "may only read"),
        ("CALL { CALL { CREATE (:B) } IN TRANSACTIONS } IN TRANSACTIONS", "nested"),
        ("MATCH (n) CALL { WITH n CALL { CREATE (:B) } IN TRANSACTIONS } RETURN 1", "nested"),
        ("CALL { CREATE (:B) } IN TRANSACTIONS UNION CALL { CREATE (:B) } IN TRANSACTIONS", "UNION"),
        ("CALL { CREATE (:B) } IN TRANSACTIONS CALL { CREATE (:C) } IN TRANSACTIONS", "only one"),
        ("PROFILE CALL { CREATE (:B) } IN TRANSACTIONS", "PROFILE"),
        ("UNWIND [1] AS i CALL { WITH j CREATE (:B) } IN TRANSACTIONS", "`j`"),
    ] {
        let e = err(&db, q).to_string();
        assert!(e.contains(want), "{q}: {e}");
    }
    assert_eq!(int(&db, "MATCH (n) RETURN count(n)"), 0);
    // Not in an explicit transaction.
    let mut tx = db.begin().unwrap();
    let e = tx.execute("CALL { CREATE (:B) } IN TRANSACTIONS").unwrap_err().to_string();
    assert!(e.contains("explicit transaction"), "{e}");
    tx.commit().unwrap();
    // EXPLAIN plans it.
    let r = db.execute("EXPLAIN UNWIND [1] AS i CALL { WITH i CREATE (:B) } IN TRANSACTIONS").unwrap();
    assert!(r.plan.is_some());
    assert_eq!(int(&db, "MATCH (n) RETURN count(n)"), 0);
}

#[test]
fn timeouts_stop_between_batches() {
    let db = mem();
    let options = QueryOptions::default().with_timeout(Duration::from_millis(200));
    // Each batch is slow; the timeout stops the query, keeping what committed.
    let q = "UNWIND range(1, 100000) AS i
             CALL { WITH i CREATE (:T) WITH i UNWIND range(1, 2000) AS j WITH count(*) AS c RETURN c } IN TRANSACTIONS OF 10 ROWS
             RETURN count(*)";
    let e = db.execute_with_options(q, &Default::default(), &options).unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e}");
    let made = int(&db, "MATCH (t:T) RETURN count(t)");
    assert!(made > 0 && made < 100000 && made % 10 == 0, "{made}");
}

#[test]
fn star_and_empty_input() {
    let db = mem();
    let r = db.execute("CALL { CREATE (n:Q {v: 1}) RETURN n.v AS v } IN TRANSACTIONS RETURN *").unwrap();
    assert_eq!(r.columns, ["v"]);
    assert_eq!(r.rows, [[ResultValue::Int(1)]]);
    let r = db.execute("UNWIND [1, 2] AS i CALL { WITH i CREATE (:Q) } IN TRANSACTIONS RETURN *").unwrap();
    assert_eq!(r.columns, ["i"]);
    assert_eq!(r.rows.len(), 2);
    let r = db
        .execute("UNWIND [] AS i CALL { WITH i CREATE (n:Z) RETURN n.x AS x } IN TRANSACTIONS RETURN count(x) AS n")
        .unwrap();
    assert_eq!(r.rows, [[ResultValue::Int(0)]]);
    assert!(!r.stats.contains_updates());
}

#[test]
fn load_csv_in_batches() {
    let dir = TempDir::new("csv");
    fs::create_dir_all(&dir.0).unwrap();
    let mut csv = String::from("id,name\n");
    for i in 0..2500 {
        csv.push_str(&format!("{i},p{i}\n"));
    }
    fs::write(dir.0.join("people.csv"), csv).unwrap();
    let db = mem();
    db.set_import_dir(Some(dir.0.clone()));
    let r = db
        .execute(
            "LOAD CSV WITH HEADERS FROM 'file:///people.csv' AS row
             CALL (row) { CREATE (:Person {id: toInteger(row.id), name: row.name}) } IN TRANSACTIONS OF 1000 ROWS",
        )
        .unwrap();
    assert_eq!(r.stats.nodes_created, 2500);
    assert_eq!(int(&db, "MATCH (p:Person) RETURN max(p.id)"), 2499);
}
