//! Text and lookup indexes, end to end: `CREATE TEXT INDEX` and `CREATE LOOKUP INDEX` on
//! every store, index seeks for `STARTS WITH`, `ENDS WITH` and `CONTAINS` on nodes and
//! relationships (the same rows as without the index), updates, and transactions.

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
            "graphbuff-texttest-{}-{}-{name}",
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

/// Words made of a few syllables, some with non-ASCII letters, and values that are not
/// strings (not indexed).
fn words(db: &Db) {
    run(
        db,
        "WITH ['ka', 'lo', 'mé', 'ri', 'su', 'ün', 'ta'] AS s
         UNWIND range(0, 1999) AS i
         CREATE (:Word {k: i, w: s[i % 7] + s[(i / 7) % 7] + s[(i / 49) % 7] + CASE WHEN i % 5 = 0 THEN '' ELSE s[i % 3] END})",
    );
    run(db, "CREATE (:Word {k: -1, w: 42}), (:Word {k: -2}), (:Other {w: 'kalomé'}), (:Word {k: -3, w: ''})");
}

const QUERIES: [&str; 12] = [
    "MATCH (n:Word) WHERE n.w STARTS WITH 'kalo' RETURN n.k ORDER BY n.k",
    "MATCH (n:Word) WHERE n.w ENDS WITH 'mésu' RETURN n.k ORDER BY n.k",
    "MATCH (n:Word) WHERE n.w CONTAINS 'ünta' RETURN n.k ORDER BY n.k",
    "MATCH (n:Word) WHERE n.w CONTAINS 'é' RETURN count(n)",
    "MATCH (n:Word) WHERE n.w ENDS WITH 'a' AND n.w STARTS WITH 'su' RETURN n.k ORDER BY n.k",
    "MATCH (n:Word) WHERE n.w CONTAINS '' RETURN count(n)",
    "MATCH (n:Word) WHERE n.w CONTAINS 'zzz' RETURN n.k",
    "MATCH (n:Word) WHERE n.w STARTS WITH null RETURN n.k",
    "MATCH (n:Word) WHERE n.w CONTAINS 42 RETURN n.k",
    "MATCH (n:Word) WHERE n.w ENDS WITH 'ka' OR n.k = 3 RETURN n.k ORDER BY n.k",
    "MATCH (n:Word) WHERE n.w CONTAINS toUpper('ka') RETURN n.k",
    "UNWIND ['rimé', 'tasu'] AS x MATCH (n:Word) WHERE n.w CONTAINS x RETURN x, count(n) ORDER BY x",
];

#[test]
fn seeks_find_the_same_rows_as_scans() {
    let plain = mem();
    words(&plain);
    let want: Vec<Vec<String>> = QUERIES.iter().map(|q| rows(&plain, q)).collect();
    assert!(want.iter().filter(|r| !r.is_empty()).count() >= 8, "{want:?}");
    on_every_store(|store, db, reopen| {
        run(&db, "CREATE TEXT INDEX words FOR (n:Word) ON (n.w)");
        words(&db);
        run(&db, "CREATE LOOKUP INDEX FOR (n) ON EACH labels(n)");
        let db = reopen(db);
        for (q, want) in QUERIES.iter().zip(&want) {
            assert_eq!(&rows(&db, q), want, "{store}: {q}");
        }
        assert_eq!(db.cached_indexes(), 1, "{store}: the seeks read the text index");
    });
}

#[test]
fn explain_shows_the_seek() {
    let db = mem();
    words(&db);
    let q = "MATCH (n:Word) WHERE n.w ENDS WITH $s RETURN n";
    assert!(!db.explain(q).unwrap().contains("NodeIndexSeek"));
    run(&db, "CREATE TEXT INDEX FOR (n:Word) ON (n.w)");
    let plan = db.explain(q).unwrap();
    assert!(plan.contains("NodeIndexSeek (n:Word) on :Word(w) ENDS WITH"), "{plan}");
    for (op, text) in [("CONTAINS", "CONTAINS"), ("STARTS WITH", "STARTS WITH")] {
        let plan = db.explain(&format!("MATCH (n:Word) WHERE n.w {op} $s RETURN n")).unwrap();
        assert!(plan.contains(&format!("on :Word(w) {text}")), "{plan}");
    }
    // A range index answers STARTS WITH, but not ENDS WITH or CONTAINS; a text index
    // answers no equality.
    run(&db, "CREATE INDEX FOR (o:Other) ON (o.w)");
    assert!(db.explain("MATCH (o:Other) WHERE o.w STARTS WITH $s RETURN o").unwrap().contains("NodeIndexSeek"));
    assert!(!db.explain("MATCH (o:Other) WHERE o.w CONTAINS $s RETURN o").unwrap().contains("NodeIndexSeek"));
    assert!(!db.explain("MATCH (n:Word) WHERE n.w = $s RETURN n").unwrap().contains("NodeIndexSeek"));
}

#[test]
fn definitions_options_and_errors() {
    on_every_store(|store, db, reopen| {
        let r = run(&db, "CREATE TEXT INDEX titles FOR (m:Movie) ON (m.title) OPTIONS {indexProvider: 'text-2.0'}");
        assert_eq!(r.stats.indexes_added, 1);
        run(&db, "CREATE TEXT INDEX FOR ()-[r:REVIEWED]-() ON (r.summary)");
        run(&db, "CREATE LOOKUP INDEX FOR (n) ON EACH labels(n)");
        run(&db, "CREATE LOOKUP INDEX rel_types FOR ()-[r]-() ON EACH type(r)");
        run(&db, "CREATE INDEX FOR (m:Movie) ON (m.title)");
        let db = reopen(db);
        let want = [
            "\"index_Movie_title\", \"NODE\", \"Movie\", [\"title\"], \"RANGE\", {}",
            "\"lookup_node_labels\", \"NODE\", null, null, \"LOOKUP\", {indexConfig: {}}",
            "\"rel_types\", \"RELATIONSHIP\", null, null, \"LOOKUP\", {indexConfig: {}}",
            "\"text_REVIEWED_summary\", \"RELATIONSHIP\", \"REVIEWED\", [\"summary\"], \"TEXT\", {indexConfig: {}}",
            "\"titles\", \"NODE\", \"Movie\", [\"title\"], \"TEXT\", {indexConfig: {}}",
        ];
        assert_eq!(rows(&db, "SHOW INDEXES"), want, "{store}");
        for (q, msg) in [
            ("CREATE TEXT INDEX other FOR (m:Movie) ON (m.title)", "a text index on :Movie(title) already exists"),
            ("CREATE LOOKUP INDEX other FOR (x) ON EACH labels(x)", "a lookup index on labels(n) already exists"),
            ("CREATE LOOKUP INDEX rel_types FOR ()-[r]-() ON type(r)", "an index named `rel_types` already exists"),
            ("CREATE TEXT INDEX titles FOR (x:X) ON (x.y)", "an index named `titles` already exists"),
        ] {
            let err = db.execute(q).unwrap_err().to_string();
            assert!(err.contains(msg), "{store}: {err}");
        }
        assert_eq!(run(&db, "CREATE LOOKUP INDEX x IF NOT EXISTS FOR (n) ON EACH labels(n)").stats.indexes_added, 0);
        for bad in [
            "CREATE TEXT INDEX FOR (m:Movie) ON (m.a, m.b)",
            "CREATE TEXT INDEX FOR (m:Movie) ON (m.b) OPTIONS {indexConfig: {`spatial.cartesian.min`: [1, 1]}}",
            "CREATE LOOKUP INDEX FOR (n) ON EACH labels(n) OPTIONS {indexConfig: {a: 1}}",
        ] {
            assert!(db.execute(bad).is_err(), "{store}: {bad}");
        }
        assert_eq!(run(&db, "DROP INDEX rel_types").stats.indexes_removed, 1);
        assert_eq!(run(&db, "DROP INDEX titles").stats.indexes_removed, 1);
        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 3, "{store}");
        // Label scans need no lookup index.
        run(&db, "DROP INDEX lookup_node_labels");
        run(&db, "CREATE (:Movie {title: 'x'})");
        assert_eq!(rows(&db, "MATCH (m:Movie) RETURN m.title"), ["\"x\""], "{store}");
    });
}

#[test]
fn relationship_seeks() {
    let db = mem();
    run(&db, "CREATE TEXT INDEX FOR ()-[r:SAID]-() ON (r.text)");
    run(
        &db,
        "UNWIND range(0, 99) AS i
         CREATE (:P {k: i})-[:SAID {k: i, text: 'line ' + toString(i) + ' ends'}]->(:P {k: i + 100})",
    );
    let q = "MATCH (a)-[r:SAID]->(b) WHERE r.text CONTAINS ' 4' AND r.text ENDS WITH '2 ends' RETURN r.k, a.k, b.k";
    assert!(db.explain(q).unwrap().contains("RelationshipIndexSeek"), "{}", db.explain(q).unwrap());
    assert_eq!(rows(&db, q), ["42, 42, 142"]);
}

#[test]
fn commits_update_the_index() {
    let db = mem();
    run(&db, "CREATE TEXT INDEX FOR (n:Word) ON (n.w)");
    words(&db);
    let q = "MATCH (n:Word) WHERE n.w CONTAINS 'xyzzy' RETURN n.k ORDER BY n.k";
    assert!(rows(&db, q).is_empty());
    run(&db, "MATCH (n:Word {k: 0}) SET n.w = 'a xyzzy b'");
    run(&db, "CREATE (:Word {k: 5000, w: 'xyzzy'})");
    run(&db, "MATCH (o:Other) SET o:Word, o.k = 6000, o.w = 'XXxyzzy'");
    run(&db, "MATCH (n:Word {k: -1}) SET n.w = 'xyzzy!'");
    assert_eq!(rows(&db, q), ["-1", "0", "5000", "6000"]);
    run(&db, "MATCH (n:Word {k: 5000}) DELETE n");
    run(&db, "MATCH (n:Word {k: 6000}) REMOVE n:Word");
    run(&db, "MATCH (n:Word {k: 0}) REMOVE n.w");
    run(&db, "MATCH (n:Word {k: -1}) SET n.w = 7");
    assert!(rows(&db, q).is_empty());
}

#[test]
fn transactions_see_their_own_writes() {
    let db = mem();
    run(&db, "CREATE TEXT INDEX FOR (n:Word) ON (n.w)");
    words(&db);
    let q = "MATCH (n:Word) WHERE n.w ENDS WITH 'plugh' RETURN n.k";
    assert!(rows(&db, q).is_empty());
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:Word {k: 7000, w: 'plugh'})").unwrap();
    assert_eq!(tx.execute(q).unwrap().rows.len(), 1);
    assert!(rows(&db, q).is_empty());
    tx.commit().unwrap();
    assert_eq!(rows(&db, q), ["7000"]);
}
