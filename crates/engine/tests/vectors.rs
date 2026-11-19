//! Vectors, end to end: `vector.similarity.cosine` / `euclidean`, vector indexes
//! (`CREATE VECTOR INDEX`, `db.index.vector.queryNodes` / `queryRelationships`) on every
//! store, and their agreement with `ORDER BY similarity LIMIT k`.

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
            "graphbuff-vtest-{}-{}-{name}",
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

fn eval(db: &Db, expr: &str) -> String {
    let r = rows(db, &format!("RETURN {expr}"));
    assert_eq!(r.len(), 1, "{expr}");
    r[0].clone()
}

#[test]
fn similarity_functions() {
    let db = mem();
    let e = |x: &str| eval(&db, x);
    // Neo4j's normalization: cosine (1 + cos) / 2, euclidean 1 / (1 + d²).
    assert_eq!(e("vector.similarity.cosine([1, 0], [2, 0])"), "1.0");
    assert_eq!(e("vector.similarity.cosine([1, 0], [0, 3.5])"), "0.5");
    assert_eq!(e("vector.similarity.cosine([1.0, 0.0], [-1, 0])"), "0.0");
    assert_eq!(e("vector.similarity.euclidean([0, 0], [3, 4])"), (1.0f64 / 26.0).to_string());
    assert_eq!(e("vector.similarity.euclidean([1, 2, 3], [1, 2, 3])"), "1.0");
    assert_eq!(e("VECTOR.SIMILARITY.COSINE([1, 1], [1, 1])"), "1.0");
    // Null in, null out; a zero vector has no cosine.
    assert_eq!(e("vector.similarity.cosine(null, [1])"), "null");
    assert_eq!(e("vector.similarity.euclidean([1], null)"), "null");
    assert_eq!(e("vector.similarity.cosine([0, 0], [1, 1])"), "null");
    // On properties.
    run(&db, "CREATE (:V {e: [0.6, 0.8]})");
    assert_eq!(
        rows(&db, "MATCH (v:V) RETURN vector.similarity.cosine(v.e, [0.6, 0.8])"),
        ["1.0"]
    );

    for (q, want) in [
        ("RETURN vector.similarity.cosine([1, 2], [1, 2, 3])", "different dimensions"),
        ("RETURN vector.similarity.cosine([], [])", "at least one element"),
        ("RETURN vector.similarity.cosine([1, null], [1, 2])", "INTEGER or FLOAT"),
        ("RETURN vector.similarity.cosine(['a'], [1])", "INTEGER or FLOAT"),
        ("RETURN vector.similarity.euclidean('x', [1])", "LIST<INTEGER | FLOAT>"),
        ("RETURN vector.similarity.cosine([1])", "expects 2 argument"),
    ] {
        let err = db.execute(q).unwrap_err().to_string();
        assert!(err.contains(want), "{q}: {err}");
    }
    assert!(matches!(db.execute("RETURN vector.similarity.manhattan([1], [1])"), Err(Error::Unsupported(_))));
}

const DOC_INDEX: &str = "CREATE VECTOR INDEX doc_emb FOR (d:Doc) ON d.emb OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.similarity_function`: 'euclidean'}}";

#[test]
fn vector_index_definitions() {
    on_every_store(|store, db, reopen| {
        let r = run(&db, DOC_INDEX);
        assert_eq!(r.stats.indexes_added, 1, "{store}");
        // A range index on the same property may sit next to it.
        run(&db, "CREATE INDEX doc_emb_range FOR (d:Doc) ON (d.emb)");
        run(&db, "CREATE VECTOR INDEX FOR ()-[r:NEAR]-() ON (r.v) OPTIONS {indexConfig: {`vector.dimensions`: 2}}");
        let db = reopen(db);
        let want = [
            "\"doc_emb\", \"NODE\", \"Doc\", [\"emb\"], \"VECTOR\", {indexConfig: {vector.dimensions: 3, vector.similarity_function: \"EUCLIDEAN\"}}",
            "\"doc_emb_range\", \"NODE\", \"Doc\", [\"emb\"], \"RANGE\", {}",
            "\"vector_NEAR_v\", \"RELATIONSHIP\", \"NEAR\", [\"v\"], \"VECTOR\", {indexConfig: {vector.dimensions: 2, vector.similarity_function: \"COSINE\"}}",
        ];
        assert_eq!(rows(&db, "SHOW INDEXES"), want, "{store}");
        assert_eq!(rows(&db, "CALL db.indexes() YIELD name, type RETURN name, type").len(), 3);
        // Again: refused, or nothing with IF NOT EXISTS (by name, or by schema).
        let err = db.execute(DOC_INDEX).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{store}: {err}");
        let r = run(&db, &DOC_INDEX.replace("doc_emb FOR", "doc_emb IF NOT EXISTS FOR"));
        assert_eq!(r.stats.indexes_added, 0);
        let r = run(&db, &DOC_INDEX.replace("doc_emb FOR", "other IF NOT EXISTS FOR"));
        assert_eq!(r.stats.indexes_added, 0);
        let err = db.execute(&DOC_INDEX.replace("doc_emb FOR", "other FOR")).unwrap_err().to_string();
        assert!(err.contains("a vector index on :Doc(emb) already exists"), "{store}: {err}");
        // Rolled back with its transaction.
        let mut tx = db.begin().unwrap();
        tx.execute("DROP INDEX doc_emb").unwrap();
        drop(tx);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 3, "{store}");
        let r = run(&db, "DROP INDEX doc_emb");
        assert_eq!(r.stats.indexes_removed, 1);
        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 2, "{store}");
    });
}

#[test]
fn bad_vector_index_options() {
    let db = mem();
    for (q, want) in [
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e", "needs OPTIONS"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 0}}", "from 1 to 4096"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 4097}}", "from 1 to 4096"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: '3'}}", "from 1 to 4096"),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.similarity_function`: 'dot'}}",
            "'cosine' or 'euclidean'",
        ),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {colour: 'red'}", "unknown index option"),
        ("CREATE INDEX r FOR (d:Doc) ON (d.e) OPTIONS {indexConfig: {`vector.dimensions`: 3}}", "for a range index"),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.quantization.enabled`: true}}",
            "not supported",
        ),
    ] {
        let err = db.execute(q).unwrap_err().to_string();
        assert!(err.contains(want), "{q}: {err}");
    }
    // Parameters and the index provider are accepted.
    let params = [(
        "o".to_string(),
        graphbuff::Value::Map(
            [
                ("indexProvider".to_string(), graphbuff::Value::String("vector-2.0".into())),
                (
                    "indexConfig".to_string(),
                    graphbuff::Value::Map([("vector.dimensions".to_string(), graphbuff::Value::Int(8))].into()),
                ),
            ]
            .into(),
        ),
    )]
    .into();
    db.execute_with_params("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS $o", &params).unwrap();
    run(&db, "CREATE INDEX r FOR (d:Doc) ON (d.e) OPTIONS {indexProvider: 'range-1.0'}");
}
