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

/// Small deterministic generator for the differential tests.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    /// A coordinate from a small set, so that ties happen.
    fn coord(&mut self) -> f64 {
        [-2.0, -1.0, -0.5, 0.0, 0.5, 1.0, 2.0, 0.25][(self.next() % 8) as usize]
    }
}

fn vec_lit(v: &[f64]) -> String {
    format!("[{}]", v.iter().map(|x| format!("{x:?}")).collect::<Vec<_>>().join(", "))
}

/// `queryNodes` gives exactly what `ORDER BY similarity DESC, id LIMIT k` gives over the
/// entities the index covers.
fn same_as_order_by(db: &Db, function: &str, dims: usize, q: &[f64], k: usize) {
    let ql = vec_lit(q);
    let via_index = rows(
        db,
        &format!("CALL db.index.vector.queryNodes('v', {k}, {ql}) YIELD node, score RETURN id(node), score"),
    );
    let validity = if function.ends_with("cosine") { "AND any(x IN n.e WHERE x <> 0)" } else { "" };
    let via_scan = rows(
        db,
        &format!(
            "MATCH (n:V) WHERE valueType(n.e) STARTS WITH 'LIST' AND size(n.e) = {dims} \
             AND all(x IN n.e WHERE valueType(x) IN ['INTEGER NOT NULL', 'FLOAT NOT NULL']) {validity} \
             WITH n, {function}(n.e, {ql}) AS score \
             RETURN id(n), score ORDER BY score DESC, id(n) LIMIT {k}"
        ),
    );
    assert_eq!(via_index, via_scan, "{function}: q = {ql}, k = {k}");
}

#[test]
fn query_nodes_on_every_store() {
    on_every_store(|store, db, reopen| {
        run(&db, DOC_INDEX);
        run(
            &db,
            "CREATE (:Doc {t: 'a', emb: [1, 0, 0]}), (:Doc {t: 'b', emb: [0.9, 0.1, 0]}), \
             (:Doc {t: 'c', emb: [0, 1, 0]}), (:Doc {t: 'short', emb: [1, 0]}), (:Doc {t: 'text', emb: 'x'}), \
             (:Other {t: 'other', emb: [1, 0, 0]})",
        );
        let q = "CALL db.index.vector.queryNodes('doc_emb', 2, [1, 0, 0]) YIELD node, score RETURN node.t, score";
        let b = format!("\"b\", {}", 1.0 / (1.0 + (0.9f64 - 1.0).powi(2) + 0.1f64.powi(2)));
        assert_eq!(rows(&db, q), ["\"a\", 1.0".to_string(), b.clone()], "{store}");
        // A commit changes the answer (the cached vectors are of the last commit).
        run(&db, "MATCH (d:Doc {t: 'c'}) SET d.emb = [1, 0, 0.0]");
        assert_eq!(rows(&db, q), ["\"a\", 1.0", "\"c\", 1.0"], "{store}");
        run(&db, "MATCH (d:Doc {t: 'a'}) REMOVE d:Doc");
        assert_eq!(rows(&db, q), ["\"c\", 1.0".to_string(), b.clone()], "{store}");
        // Inside a transaction: its own writes.
        let mut tx = db.begin().unwrap();
        tx.execute("CREATE (:Doc {t: 'new', emb: [1, 0, 0]})").unwrap();
        let r = tx.execute(q).unwrap();
        assert_eq!(r.rows.len(), 2, "{store}");
        assert_eq!(r.rows[1][0].to_string(), "\"new\"", "{store}");
        drop(tx);
        let db = reopen(db);
        assert_eq!(rows(&db, q), ["\"c\", 1.0".to_string(), b], "{store}");
        // k larger than the index: all of it.
        assert_eq!(rows(&db, "CALL db.index.vector.queryNodes('doc_emb', 100, [1, 0, 0]) YIELD node RETURN node").len(), 2);
    });
}

#[test]
fn query_relationships() {
    let db = mem();
    run(&db, "CREATE VECTOR INDEX near FOR ()-[r:NEAR]-() ON r.v OPTIONS {indexConfig: {`vector.dimensions`: 2}}");
    run(&db, "CREATE (a:P {n: 1})-[:NEAR {v: [1, 1]}]->(b:P {n: 2}), (a)-[:NEAR {v: [1, -1]}]->(b), (a)-[:FAR {v: [1, 1]}]->(b)");
    let r = rows(
        &db,
        "CALL db.index.vector.queryRelationships('near', 5, [2, 2]) YIELD relationship, score \
         RETURN type(relationship), relationship.v, score",
    );
    assert_eq!(r, ["\"NEAR\", [1, 1], 1.0", "\"NEAR\", [1, -1], 0.5"]);
    let err = db.execute("CALL db.index.vector.queryNodes('near', 1, [1, 1])").unwrap_err().to_string();
    assert!(err.contains("use db.index.vector.queryRelationships"), "{err}");
}

#[test]
fn bad_queries() {
    let db = mem();
    run(&db, DOC_INDEX);
    run(&db, "CREATE VECTOR INDEX cos FOR (d:Doc) ON d.c OPTIONS {indexConfig: {`vector.dimensions`: 2}}");
    run(&db, "CREATE INDEX plain FOR (d:Doc) ON (d.t)");
    for (q, want) in [
        ("CALL db.index.vector.queryNodes('nope', 1, [1, 2, 3])", "no vector index named `nope`"),
        ("CALL db.index.vector.queryNodes('plain', 1, [1, 2, 3])", "no vector index named `plain`"),
        ("CALL db.index.vector.queryNodes('doc_emb', 1, [1, 2])", "3 dimensions, the query vector 2"),
        ("CALL db.index.vector.queryNodes('doc_emb', 0, [1, 2, 3])", "at least 1"),
        ("CALL db.index.vector.queryNodes('doc_emb', 1.5, [1, 2, 3])", "must be an INTEGER"),
        ("CALL db.index.vector.queryNodes('doc_emb', 1, null)", "query vector is null"),
        ("CALL db.index.vector.queryNodes('doc_emb', 1, [1, 'x', 3])", "INTEGER or FLOAT"),
        ("CALL db.index.vector.queryNodes('cos', 1, [0, 0])", "zero vector"),
        ("CALL db.index.vector.queryNodes(1, 1, [0, 0])", "must be a STRING"),
    ] {
        let err = db.execute(q).unwrap_err().to_string();
        assert!(err.contains(want), "{q}: {err}");
    }
    // An empty index answers nothing.
    assert!(rows(&db, "CALL db.index.vector.queryNodes('doc_emb', 3, [1, 2, 3]) YIELD node RETURN node").is_empty());
}

#[test]
fn exact_like_order_by() {
    for (similarity, function) in [("cosine", "vector.similarity.cosine"), ("euclidean", "vector.similarity.euclidean")] {
        let db = mem();
        let dims = 4;
        run(
            &db,
            &format!(
                "CREATE VECTOR INDEX v FOR (n:V) ON n.e OPTIONS {{indexConfig: \
                 {{`vector.dimensions`: {dims}, `vector.similarity_function`: '{similarity}'}}}}"
            ),
        );
        let mut rng = Lcg(42);
        let mut creates = Vec::new();
        for i in 0..400 {
            let v: Vec<f64> = (0..dims).map(|_| rng.coord()).collect();
            let e = match i % 50 {
                7 => "[1, 2]".to_string(),           // wrong dimension
                9 => "[0, 0, 0, 0]".to_string(),     // no cosine
                11 => "'text'".to_string(),          // not a vector
                13 => "['a', 'b', 'c', 'd']".to_string(), // not numbers
                17 => format!("[{i}, 1, 0, 0]"),     // integers
                _ => vec_lit(&v),
            };
            creates.push(format!("(:V {{e: {e}}})"));
        }
        run(&db, &format!("CREATE {}", creates.join(", ")));
        run(&db, "CREATE (:W {e: [1, 1, 1, 1]})");
        let query = |q: &[f64], k: usize| {
            format!("CALL db.index.vector.queryNodes('v', {k}, {}) YIELD node, score RETURN id(node), score", vec_lit(q))
        };
        for round in 0..30 {
            let q: Vec<f64> = (0..dims).map(|_| rng.coord()).collect();
            if similarity == "cosine" && q.iter().all(|x| *x == 0.0) {
                continue;
            }
            let k = [1, 3, 10, 50, 1000][round % 5];
            same_as_order_by(&db, function, dims, &q, k);
            // Inside a transaction (no cached matrix) too.
            if round % 10 == 0 {
                let mut tx = db.begin().unwrap();
                let a = tx.execute(&query(&q, k)).unwrap();
                drop(tx);
                assert_eq!(a.rows, run(&db, &query(&q, k)).rows, "{similarity}: in a transaction");
            }
        }
    }
}
