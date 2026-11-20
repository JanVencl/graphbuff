//! Full-text indexes, end to end: `CREATE FULLTEXT INDEX` on every store,
//! `db.index.fulltext.queryNodes` / `queryRelationships` (query syntax, scoring order,
//! options), analyzers, and transactions.

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
            "graphbuff-fttest-{}-{}-{name}",
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

const MOVIES: &str = "CREATE
    (:Movie {k: 1, title: 'The Matrix', plot: 'A hacker learns the truth about reality'}),
    (:Movie {k: 2, title: 'The Matrix Reloaded', plot: 'Neo and the rebels fight the machines'}),
    (:Movie {k: 3, title: 'The Matrix Revolutions', plot: 'The war between humans and machines ends'}),
    (:Movie {k: 4, title: 'Speed', plot: 'A bus must keep its speed above fifty miles per hour'}),
    (:Movie {k: 5, title: 'John Wick', plot: 'A retired hitman seeks vengeance'}),
    (:Book {k: 6, title: 'Neuromancer', plot: 'A washed-up hacker is hired for one last job'}),
    (:Person {k: 7, title: 'The Matrix fan'})";

const INDEX: &str = "CREATE FULLTEXT INDEX titles FOR (n:Movie|Book) ON EACH [n.title, n.plot]";

/// The `k` of the nodes `query` finds in index `titles`, best first.
fn found(db: &Db, query: &str) -> Vec<i64> {
    let q = "CALL db.index.fulltext.queryNodes('titles', $q) YIELD node, score RETURN node.k, score";
    let params = [("q".to_string(), graphbuff::Value::String(query.into()))].into();
    let r = db.execute_with_params(q, &params).unwrap_or_else(|e| panic!("{query}: {e}"));
    let mut last = f64::INFINITY;
    r.rows
        .iter()
        .map(|row| {
            let (graphbuff::ResultValue::Int(k), graphbuff::ResultValue::Float(s)) = (&row[0], &row[1]) else { panic!("{row:?}") };
            assert!(*s > 0.0 && *s <= last, "{query}: scores not descending: {:?}", r.rows);
            last = *s;
            *k
        })
        .collect()
}

#[test]
fn fulltext_index_definitions() {
    on_every_store(|store, db, reopen| {
        let r = run(&db, INDEX);
        assert_eq!(r.stats.indexes_added, 1, "{store}");
        run(&db, "CREATE FULLTEXT INDEX FOR ()-[r:REVIEWED|RATED]-() ON EACH [r.summary] OPTIONS {indexConfig: {`fulltext.analyzer`: 'english', `fulltext.eventually_consistent`: true}}");
        // A range index on the same property may sit next to it.
        run(&db, "CREATE INDEX title_range FOR (n:Movie) ON (n.title)");
        run(&db, MOVIES);
        let db = reopen(db);
        let want = [
            "\"fulltext_REVIEWED_RATED_summary\", \"RELATIONSHIP\", \"REVIEWED|RATED\", [\"summary\"], \"FULLTEXT\", {indexConfig: {fulltext.analyzer: \"english\", fulltext.eventually_consistent: false}}",
            "\"title_range\", \"NODE\", \"Movie\", [\"title\"], \"RANGE\", {}",
            "\"titles\", \"NODE\", \"Movie|Book\", [\"title\", \"plot\"], \"FULLTEXT\", {indexConfig: {fulltext.analyzer: \"standard-no-stop-words\", fulltext.eventually_consistent: false}}",
        ];
        assert_eq!(rows(&db, "SHOW INDEXES"), want, "{store}");
        assert_eq!(found(&db, "matrix"), [1, 2, 3], "{store}");
        // Again: refused, or nothing with IF NOT EXISTS (by name, or by schema in any
        // label order).
        let err = db.execute(INDEX).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{store}: {err}");
        let r = run(&db, "CREATE FULLTEXT INDEX other IF NOT EXISTS FOR (n:Book|Movie) ON EACH [n.title, n.plot]");
        assert_eq!(r.stats.indexes_added, 0);
        let err = db.execute("CREATE FULLTEXT INDEX other FOR (n:Book|Movie) ON EACH [n.title, n.plot]").unwrap_err().to_string();
        assert!(err.contains("a full-text index on :Book|Movie(title, plot) already exists"), "{store}: {err}");
        // Other properties, or one label: another index.
        run(&db, "CREATE FULLTEXT INDEX plots FOR (n:Movie) ON EACH [n.plot]");
        // Writes after reopening are indexed too.
        run(&db, "CREATE (:Book {k: 8, title: 'Matrix Theory'})");
        assert_eq!(found(&db, "matrix theory"), [8, 1, 2, 3], "{store}");
        // Rolled back with its transaction.
        let mut tx = db.begin().unwrap();
        tx.execute("DROP INDEX titles").unwrap();
        drop(tx);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 4, "{store}");
        let r = run(&db, "DROP INDEX titles");
        assert_eq!(r.stats.indexes_removed, 1);
        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 3, "{store}");
        assert!(db.execute("CALL db.index.fulltext.queryNodes('titles', 'x')").is_err());
    });
}

#[test]
fn bad_fulltext_index_options() {
    let db = mem();
    for (q, want) in [
        ("OPTIONS {indexConfig: {`fulltext.analyzer`: 'klingon'}}", "unknown full-text analyzer 'klingon'"),
        ("OPTIONS {indexConfig: {`fulltext.analyzer`: 3}}", "does not accept"),
        ("OPTIONS {indexConfig: {`fulltext.eventually_consistent`: 'yes'}}", "does not accept"),
        ("OPTIONS {indexConfig: {`fulltext.boost`: 1}}", "not supported"),
        ("OPTIONS {indexConfig: {`vector.dimensions`: 3}}", "unknown index setting"),
        ("OPTIONS {colour: 'red'}", "unknown index option"),
    ] {
        let err = db.execute(&format!("{INDEX} {q}")).unwrap_err().to_string();
        assert!(err.contains(want), "{q}: {err}");
    }
    for (q, want) in [
        ("CREATE FULLTEXT INDEX FOR (n:A|A) ON EACH [n.x]", "repeated"),
        ("CREATE FULLTEXT INDEX FOR (n:A) ON EACH [n.x, n.x]", "repeated"),
    ] {
        let err = db.execute(q).unwrap_err().to_string();
        assert!(err.contains(want), "{q}: {err}");
    }
    assert!(rows(&db, "SHOW INDEXES").is_empty());
}

#[test]
fn query_syntax_and_ranking() {
    let db = mem();
    run(&db, INDEX);
    run(&db, MOVIES);
    for (query, want) in [
        // The shorter title ranks first; the :Person is not indexed.
        ("matrix", &[1, 2, 3][..]),
        ("MATRIX", &[1, 2, 3]),
        ("hacker", &[1, 6]),
        ("title:hacker", &[]),
        ("plot:hacker", &[1, 6]),
        ("colour:hacker", &[]),
        ("machines AND rebels", &[2]),
        ("machines && rebels", &[2]),
        ("machines -rebels", &[3]),
        ("machines NOT war", &[2]),
        ("+machines +war", &[3]),
        ("machines OR hitman", &[5, 2, 3]),
        ("\"matrix reloaded\"", &[2]),
        ("\"reloaded matrix\"", &[]),
        ("title:(matrix AND NOT reloaded)", &[1, 3]),
        ("matr*", &[1, 2, 3]),
        ("mat?ix", &[1, 2, 3]),
        ("*eloaded", &[2]),
        ("matirx~", &[1, 2, 3]),
        ("matirx~1", &[1, 2, 3]),
        ("matirx~0", &[]),
        ("speed^3 OR hacker", &[4, 1, 6]),
        ("speed OR hacker^5", &[1, 6, 4]),
        ("washed-up", &[6]),
        ("NOT matrix", &[]),
        ("*:*", &[1, 2, 3, 4, 5, 6]),
        ("nothing", &[]),
    ] {
        assert_eq!(found(&db, query), want, "{query}");
    }
    // Options: paging, and another analyzer for the query (keyword: as written).
    let q = |opts: &str| rows(&db, &format!("CALL db.index.fulltext.queryNodes('titles', 'matrix', {opts}) YIELD node RETURN node.k"));
    assert_eq!(q("{skip: 1, limit: 1}"), ["2"]);
    assert_eq!(q("{limit: 0}"), Vec::<String>::new());
    assert_eq!(q("{analyzer: 'keyword'}"), ["1", "2", "3"]);
    assert_eq!(
        rows(&db, "CALL db.index.fulltext.queryNodes('titles', 'Matrix', {analyzer: 'keyword'}) YIELD node RETURN node.k"),
        Vec::<String>::new()
    );
    // Used as a source of rows like any procedure.
    assert_eq!(
        rows(&db, "CALL db.index.fulltext.queryNodes('titles', 'matrix') YIELD node WHERE node.k > 1 RETURN count(*)"),
        ["2"]
    );
}

#[test]
fn analyzers_and_relationships() {
    let db = mem();
    run(&db, "CREATE FULLTEXT INDEX notes FOR (n:Note) ON EACH [n.text] OPTIONS {indexConfig: {`fulltext.analyzer`: 'english'}}");
    run(&db, "CREATE (:Note {k: 1, text: 'Running dogs'}), (:Note {k: 2, text: 'The runner ran'}), (:Note {k: 3, text: [\"Dog's day\", 'out']})");
    let q = |s: &str| rows(&db, &format!("CALL db.index.fulltext.queryNodes('notes', '{s}') YIELD node RETURN node.k"));
    assert_eq!(q("run"), ["1"]);
    assert_eq!(q("dog"), ["1", "3"]);
    assert_eq!(q("\"dogs day\""), ["3"]);
    // Stop words alone match nothing.
    assert_eq!(q("the"), Vec::<String>::new());
    assert_eq!(q("the OR ran"), ["2"]);

    run(&db, "CREATE FULLTEXT INDEX reviews FOR ()-[r:REVIEWED|RATED]-() ON EACH [r.summary]");
    run(&db, "CREATE (a:P)-[:REVIEWED {k: 1, summary: 'Great fun'}]->(m:M), (a)-[:RATED {k: 2, summary: 'Not much fun at all'}]->(m), (a)-[:LIKED {k: 3, summary: 'fun'}]->(m)");
    assert_eq!(
        rows(&db, "CALL db.index.fulltext.queryRelationships('reviews', 'fun') YIELD relationship RETURN relationship.k"),
        ["1", "2"]
    );
    let r = rows(&db, "CALL db.index.fulltext.listAvailableAnalyzers() YIELD analyzer, stopwords RETURN analyzer, size(stopwords)");
    assert_eq!(r.len(), 7);
    assert!(r.contains(&"\"standard-no-stop-words\", 0".to_string()), "{r:?}");
    assert!(r.contains(&"\"english\", 33".to_string()), "{r:?}");
    assert!(run(&db, "CALL db.index.fulltext.awaitEventuallyConsistentIndexRefresh()").rows.is_empty());
}

#[test]
fn bad_queries() {
    let db = mem();
    run(&db, INDEX);
    run(&db, "CREATE FULLTEXT INDEX reviews FOR ()-[r:R]-() ON EACH [r.s]");
    run(&db, "CREATE VECTOR INDEX v FOR (n:V) ON n.e OPTIONS {indexConfig: {`vector.dimensions`: 2}}");
    for (q, want) in [
        ("CALL db.index.fulltext.queryNodes('nope', 'x')", "no full-text index named `nope`"),
        ("CALL db.index.fulltext.queryNodes('v', 'x')", "no full-text index named `v`"),
        ("CALL db.index.fulltext.queryNodes('reviews', 'x')", "use db.index.fulltext.queryRelationships"),
        ("CALL db.index.fulltext.queryRelationships('titles', 'x')", "use db.index.fulltext.queryNodes"),
        ("CALL db.index.fulltext.queryNodes('titles', 'a AND')", "cannot parse full-text query"),
        ("CALL db.index.fulltext.queryNodes('titles', '(a')", "cannot parse full-text query"),
        ("CALL db.index.fulltext.queryNodes('titles', '')", "the query is empty"),
        ("CALL db.index.fulltext.queryNodes('titles', '[a TO b]')", "ranges"),
        ("CALL db.index.fulltext.queryNodes('titles', 'x', {limit: -1})", "non-negative"),
        ("CALL db.index.fulltext.queryNodes('titles', 'x', {analyzer: 'nope'})", "unknown analyzer"),
        ("CALL db.index.fulltext.queryNodes('titles', 'x', {colour: 1})", "unknown option"),
        ("CALL db.index.fulltext.queryNodes('titles', 3)", "STRING"),
    ] {
        let err = db.execute(q).unwrap_err().to_string();
        assert!(err.contains(want), "{q}: {err}");
    }
}

#[test]
fn transactions_see_their_own_writes() {
    let db = mem();
    run(&db, INDEX);
    run(&db, MOVIES);
    assert_eq!(found(&db, "matrix"), [1, 2, 3]);
    let mut tx = db.begin().unwrap();
    tx.execute("MATCH (n:Movie {k: 1}) DETACH DELETE n").unwrap();
    tx.execute("CREATE (:Book {k: 9, title: 'Matrix'})").unwrap();
    let r = tx
        .execute("CALL db.index.fulltext.queryNodes('titles', 'matrix') YIELD node RETURN node.k")
        .unwrap();
    let ks: Vec<String> = r.rows.iter().map(|r| r[0].to_string()).collect();
    assert_eq!(ks, ["9", "2", "3"]);
    // Not seen outside until committed.
    assert_eq!(found(&db, "matrix"), [1, 2, 3]);
    tx.commit().unwrap();
    assert_eq!(found(&db, "matrix"), [9, 2, 3]);
}
