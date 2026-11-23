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
            "\"doc_emb\", \"NODE\", \"Doc\", [\"emb\"], \"VECTOR\", {indexConfig: {vector.dimensions: 3, vector.hnsw.enabled: false, vector.similarity_function: \"EUCLIDEAN\"}}",
            "\"doc_emb_range\", \"NODE\", \"Doc\", [\"emb\"], \"RANGE\", {}",
            "\"vector_NEAR_v\", \"RELATIONSHIP\", \"NEAR\", [\"v\"], \"VECTOR\", {indexConfig: {vector.dimensions: 2, vector.hnsw.enabled: false, vector.similarity_function: \"COSINE\"}}",
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
fn filter_properties() {
    on_every_store(|store, db, reopen| {
        run(
            &db,
            "CREATE VECTOR INDEX docs FOR (d:Doc) ON d.emb WITH [d.tenant, d.year] OPTIONS {indexConfig: {`vector.dimensions`: 2}}",
        );
        run(
            &db,
            "CREATE VECTOR INDEX FOR ()-[r:NEAR]-() ON (r.v) WITH [r.since] OPTIONS {indexConfig: {`vector.dimensions`: 2, `vector.hnsw.enabled`: true}}",
        );
        run(&db, "CREATE (:Doc {t: 'a', emb: [1, 0], tenant: 1})-[:NEAR {v: [1, 0], since: 2}]->(:Doc {t: 'b', emb: [0, 1]})");
        let db = reopen(db);
        assert_eq!(
            rows(&db, "CALL db.indexes() YIELD name, labelOrType, properties RETURN name, labelOrType, properties ORDER BY name"),
            [
                "\"docs\", \"Doc\", [\"emb\", \"tenant\", \"year\"]",
                // Named after the vector property, as without filter properties.
                "\"vector_NEAR_v\", \"NEAR\", [\"v\", \"since\"]",
            ],
            "{store}"
        );
        // An entity without the filter properties is indexed too.
        let q = "CALL db.index.vector.queryNodes('docs', 5, [0, 1]) YIELD node RETURN node.t";
        assert_eq!(rows(&db, q), ["\"b\"", "\"a\""], "{store}");
        // Another vector index on the same property is refused, whatever its filters.
        let again = "CREATE VECTOR INDEX other FOR (d:Doc) ON d.emb WITH [d.lang] OPTIONS {indexConfig: {`vector.dimensions`: 2}}";
        let err = db.execute(again).unwrap_err().to_string();
        assert!(err.contains("a vector index on :Doc(emb) already exists"), "{store}: {err}");
        let r = run(&db, &again.replace("other FOR", "other IF NOT EXISTS FOR"));
        assert_eq!(r.stats.indexes_added, 0, "{store}");
        // The filter properties answer no seeks, and a range index may be on them.
        run(&db, "CREATE INDEX FOR (d:Doc) ON (d.tenant)");
        assert_eq!(rows(&db, "MATCH (d:Doc {tenant: 1}) RETURN d.t"), ["\"a\""], "{store}");
        run(&db, "DROP INDEX docs");
        let db = reopen(db);
        assert_eq!(rows(&db, "SHOW INDEXES").len(), 2, "{store}");
    });
}

#[test]
fn bad_vector_index_options() {
    let db = mem();
    for (q, want) in [
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON (d.e, d.f) OPTIONS {indexConfig: {`vector.dimensions`: 3}}", "exactly one property"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e WITH [d.e] OPTIONS {indexConfig: {`vector.dimensions`: 3}}", "`e` is repeated"),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e WITH [d.a, d.b, d.a] OPTIONS {indexConfig: {`vector.dimensions`: 3}}",
            "`a` is repeated",
        ),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e WITH [d.p1, d.p2, d.p3, d.p4, d.p5, d.p6, d.p7, d.p8, d.p9, d.p10, d.p11, d.p12, \
             d.p13, d.p14, d.p15, d.p16] OPTIONS {indexConfig: {`vector.dimensions`: 3}}",
            "at most 16 properties",
        ),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e", "needs OPTIONS"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 0}}", "from 1 to 4096"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 4097}}", "from 1 to 4096"),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: '3'}}", "from 1 to 4096"),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.similarity_function`: 'dot'}}",
            "'cosine' or 'euclidean'",
        ),
        ("CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {colour: 'red'}", "unknown index option"),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.hnsw.m`: 1}}",
            "from 2 to 512",
        ),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.hnsw.ef_construction`: 3201}}",
            "from 1 to 3200",
        ),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.hnsw.enabled`: 'yes'}}",
            "needs a BOOLEAN",
        ),
        (
            "CREATE VECTOR INDEX v FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.hnsw.enabled`: false, `vector.hnsw.m`: 8}}",
            "need `vector.hnsw.enabled: true`",
        ),
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

/// The same differential check with the scores computed on the GPU (f32 sums), on
/// tie-heavy data. Skipped without a usable GPU.
#[cfg(feature = "gpu")]
#[test]
fn exact_on_the_gpu() {
    let gpu = match graphbuff_gpu::WgpuKernels::new() {
        Ok(k) => std::sync::Arc::new(k.with_min_work(0)),
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    for (similarity, function) in [("cosine", "vector.similarity.cosine"), ("euclidean", "vector.similarity.euclidean")] {
        for dims in [4, 48] {
            let db = mem();
            db.set_kernels(gpu.clone());
            run(
                &db,
                &format!(
                    "CREATE VECTOR INDEX v FOR (n:V) ON n.e OPTIONS {{indexConfig: \
                     {{`vector.dimensions`: {dims}, `vector.similarity_function`: '{similarity}'}}}}"
                ),
            );
            let mut rng = Lcg(7 + dims as u64);
            let creates: Vec<String> = (0..600)
                .map(|_| format!("(:V {{e: {}}})", vec_lit(&(0..dims).map(|_| rng.coord()).collect::<Vec<_>>())))
                .collect();
            run(&db, &format!("CREATE {}", creates.join(", ")));
            let before = gpu.gpu_runs();
            for round in 0..12 {
                let q: Vec<f64> = (0..dims).map(|_| rng.coord()).collect();
                if q.iter().all(|x| *x == 0.0) {
                    continue;
                }
                same_as_order_by(&db, function, dims, &q, [1, 5, 40, 1000][round % 4]);
            }
            assert!(gpu.gpu_runs() > before, "{similarity}: the scores ran on the GPU");
        }
    }
}

/// `queryRelationships` on index `r` (type `NEAR`, property `e`) gives what `ORDER BY` gives.
fn rels_same_as_order_by(db: &Db, function: &str, dims: usize, q: &[f64], k: usize) {
    let ql = vec_lit(q);
    let via_index = rows(
        db,
        &format!(
            "CALL db.index.vector.queryRelationships('r', {k}, {ql}) YIELD relationship, score \
             RETURN id(relationship), score"
        ),
    );
    let validity = if function.ends_with("cosine") { "AND any(x IN r.e WHERE x <> 0)" } else { "" };
    let via_scan = rows(
        db,
        &format!(
            "MATCH ()-[r:NEAR]->() WHERE valueType(r.e) STARTS WITH 'LIST' AND size(r.e) = {dims} \
             AND all(x IN r.e WHERE valueType(x) IN ['INTEGER NOT NULL', 'FLOAT NOT NULL']) {validity} \
             WITH r, {function}(r.e, {ql}) AS score \
             RETURN id(r), score ORDER BY score DESC, id(r) LIMIT {k}"
        ),
    );
    assert_eq!(via_index, via_scan, "{function}: q = {ql}, k = {k}");
}

/// Random commits of every kind of change a vector index sees: new, changed, invalid and
/// deleted vectors (of nodes and relationships), labels added and removed, nodes deleted
/// with their relationships, rolled-back transactions, and batches larger than the
/// matrix's free rows or deleting most of it. Queries after some commits only, so that
/// the changes of several commits apply at once too; each answer is checked against
/// `ORDER BY`. `reopen` (when given) closes and reopens the database halfway.
fn churn(mut db: Db, function: &str, similarity: &str, dims: usize, seed: u64, reopen: Option<&dyn Fn(Db) -> Db>) {
    for (name, target) in [("v", "(n:V) ON n.e"), ("r", "()-[n:NEAR]-() ON n.e")] {
        run(
            &db,
            &format!(
                "CREATE VECTOR INDEX {name} FOR {target} OPTIONS {{indexConfig: \
                 {{`vector.dimensions`: {dims}, `vector.similarity_function`: '{similarity}'}}}}"
            ),
        );
    }
    let mut rng = Lcg(seed);
    let computed = |salt: u64| format!("[x IN range(1, {dims}) | toFloat((id(n) * x + {salt}) % 7) - 3.0]");
    let invalid = ["null", "[1, 2]", "'text'", &format!("[x IN range(1, {dims}) | 0]"), &format!("[x IN range(1, {dims}) | 'a']")];
    for round in 0..80 {
        let m = rng.next() % 11 + 2;
        let r = rng.next() % m;
        let salt = rng.next();
        let q = match rng.next() % 12 {
            0 => {
                let e = vec_lit(&(0..dims).map(|_| rng.coord()).collect::<Vec<_>>());
                format!("CREATE (:V {{e: {e}}}), (:W {{e: {e}}}), (:V {{e: [1]}}), (n:V) SET n.e = {}", computed(salt))
            }
            1 => format!("MATCH (n) WHERE id(n) % {m} = {r} SET n.e = {}", computed(salt)),
            2 => format!("MATCH (n:V) WHERE id(n) % {m} = {r} REMOVE n:V"),
            3 => format!("MATCH (n) WHERE id(n) % {m} = {r} SET n:V"),
            4 => format!("MATCH (n) WHERE id(n) % {} = {r} DETACH DELETE n", m + 20),
            5 => format!("MATCH (n) WHERE id(n) % {m} = {r} SET n.e = {}", invalid[(salt % 5) as usize]),
            6 => format!(
                "MATCH (a), (b) WHERE id(a) % {m} = {r} AND id(b) % {} = 1 WITH a, b LIMIT 12 \
                 CREATE (a)-[n:NEAR]->(b) SET n.e = {}",
                m + 3,
                computed(salt)
            ),
            7 => format!("MATCH ()-[n:NEAR]->() WHERE id(n) % {m} = {r} SET n.e = {}", computed(salt)),
            8 => format!("MATCH ()-[n:NEAR]->() WHERE id(n) % {} = {r} DELETE n", m + 5),
            9 => {
                // Rolled back.
                let mut tx = db.begin().unwrap();
                tx.execute(&format!("MATCH (n) WHERE id(n) % {m} = {r} SET n.e = {}, n:V", computed(salt))).unwrap();
                tx.execute("UNWIND range(1, 30) AS i CREATE (n:V {e: [1]})").unwrap();
                tx.rollback();
                continue;
            }
            10 => format!("UNWIND range(1, {}) AS i CREATE (n:V) SET n.e = {}", 40 + salt % 120, computed(salt)),
            _ if salt.is_multiple_of(4) => "MATCH (n:V) WITH n ORDER BY id(n) LIMIT 150 DETACH DELETE n".to_string(),
            _ => "MATCH (n:V) WITH n LIMIT 1 SET n.e = n.e".to_string(),
        };
        run(&db, &q);
        if round == 40
            && let Some(reopen) = reopen
        {
            db = reopen(db);
        }
        if rng.next().is_multiple_of(3) {
            continue;
        }
        for _ in 0..2 {
            let q: Vec<f64> = (0..dims).map(|_| rng.coord()).collect();
            if q.iter().all(|x| *x == 0.0) {
                continue;
            }
            let k = [1, 4, 30, 1000][(rng.next() % 4) as usize];
            same_as_order_by(&db, function, dims, &q, k);
            rels_same_as_order_by(&db, function, dims, &q, k);
        }
    }
}

#[test]
fn kept_up_to_date_with_commits() {
    on_every_store(|store, db, reopen| {
        eprintln!("{store}");
        churn(db, "vector.similarity.cosine", "cosine", 3, 11, Some(reopen));
    });
    churn(mem(), "vector.similarity.euclidean", "euclidean", 5, 12, None);
    churn(mem(), "vector.similarity.cosine", "cosine", 2, 13, None);
}

/// The same with the scores on the GPU, whose copy of the vectors is patched with the
/// rows each commit changed. Skipped without a usable GPU.
#[cfg(feature = "gpu")]
#[test]
fn kept_up_to_date_on_the_gpu() {
    let gpu = match graphbuff_gpu::WgpuKernels::new() {
        Ok(k) => std::sync::Arc::new(k.with_min_work(0)),
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    for (similarity, function, dims) in [("cosine", "vector.similarity.cosine", 3), ("euclidean", "vector.similarity.euclidean", 6)] {
        let db = mem();
        db.set_kernels(gpu.clone());
        let before = gpu.gpu_runs();
        churn(db, function, similarity, dims, 21, None);
        assert!(gpu.gpu_runs() > before, "{similarity}: the scores ran on the GPU");
    }
}

/// The `options` `SHOW INDEXES` lists for index `name`.
fn show(db: &Db, name: &str) -> String {
    let line = rows(db, "SHOW INDEXES").into_iter().find(|r| r.starts_with(&format!("\"{name}\","))).unwrap();
    line[line.find('{').unwrap()..].to_string()
}

#[test]
fn approximate_index_definitions() {
    on_every_store(|store, db, reopen| {
        run(
            &db,
            "CREATE VECTOR INDEX a FOR (d:A) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.hnsw.m`: 8, `vector.hnsw.ef_construction`: 50}}",
        );
        run(&db, "CREATE VECTOR INDEX b FOR (d:B) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.hnsw.enabled`: true}}");
        run(&db, "CREATE (:A {e: [1, 0, 0, 0]}), (:B {e: [0, 1, 0, 0]})");
        let db = reopen(db);
        assert_eq!(
            show(&db, "a"),
            "{indexConfig: {vector.dimensions: 4, vector.hnsw.ef_construction: 50, vector.hnsw.enabled: true, vector.hnsw.m: 8, vector.similarity_function: \"COSINE\"}}",
            "{store}"
        );
        assert_eq!(
            show(&db, "b"),
            "{indexConfig: {vector.dimensions: 4, vector.hnsw.ef_construction: 100, vector.hnsw.enabled: true, vector.hnsw.m: 16, vector.similarity_function: \"COSINE\"}}",
            "{store}"
        );
        assert_eq!(rows(&db, "CALL db.index.vector.queryNodes('a', 3, [1, 0, 0, 0]) YIELD score RETURN score"), ["1.0"], "{store}");
    });
}

/// `id, score` of the `k` nearest by node index `name`.
fn nearest(db: &Db, name: &str, k: usize, q: &[f64]) -> Vec<String> {
    rows(db, &format!("CALL db.index.vector.queryNodes('{name}', {k}, {}) YIELD node, score RETURN id(node), score", vec_lit(q)))
}

/// The share of the exact index's results that the approximate one finds, over `queries`;
/// every score the approximate index gives is the exact score.
fn recall(db: &Db, queries: &[Vec<f64>], k: usize) -> f64 {
    let mut hits = 0;
    for q in queries {
        let exact = nearest(db, "exact", k, q);
        let approx = nearest(db, "approx", k, q);
        let id = |r: &String| r.split(", ").next().unwrap().to_string();
        let exact_ids: Vec<String> = exact.iter().map(id).collect();
        for r in &approx {
            let score = rows(db, &format!("MATCH (n) WHERE id(n) = {} RETURN vector.similarity.cosine(n.e, {})", id(r), vec_lit(q)));
            assert_eq!(r.split(", ").nth(1).unwrap(), score[0]);
        }
        hits += approx.iter().filter(|r| exact_ids.contains(&id(r))).count();
    }
    hits as f64 / (queries.len() * k) as f64
}

#[test]
fn approximate_search() {
    let db = mem();
    let dims = 16;
    run(&db, &format!("CREATE VECTOR INDEX exact FOR (n:X) ON n.e OPTIONS {{indexConfig: {{`vector.dimensions`: {dims}}}}}"));
    run(
        &db,
        &format!("CREATE VECTOR INDEX approx FOR (n:H) ON n.e OPTIONS {{indexConfig: {{`vector.dimensions`: {dims}, `vector.hnsw.enabled`: true}}}}"),
    );
    let mut g = Lcg(42);
    let mut point = || (0..dims).map(|_| (g.next() % 2001) as f64 / 1000.0 - 1.0).collect::<Vec<f64>>();
    let vectors: Vec<String> = (0..1500).map(|_| vec_lit(&point())).collect();
    run(&db, &format!("UNWIND [{}] AS e CREATE (:X:H {{e: e}})", vectors.join(", ")));
    let queries: Vec<Vec<f64>> = (0..20).map(|_| point()).collect();
    let r = recall(&db, &queries, 10);
    assert!(r >= 0.9, "recall {r}");

    // A changed vector is found with its new value; an entity that left the index is gone.
    let q = queries[0].clone();
    let first = rows(&db, "MATCH (n:H) RETURN id(n) ORDER BY id(n) LIMIT 1")[0].clone();
    run(&db, &format!("MATCH (n) WHERE id(n) = {first} SET n.e = {}", vec_lit(&q)));
    assert_eq!(nearest(&db, "approx", 1, &q), [format!("{first}, 1.0")]);
    run(&db, &format!("MATCH (n) WHERE id(n) = {first} REMOVE n:H"));
    assert!(nearest(&db, "approx", 5, &q).iter().all(|r| !r.starts_with(&format!("{first},"))));
    // Inside a transaction the index is searched exactly, as the transaction sees it.
    let mut tx = db.begin().unwrap();
    tx.execute(&format!("MATCH (n) WHERE id(n) = {first} SET n:H")).unwrap();
    let r = tx
        .execute(&format!("CALL db.index.vector.queryNodes('approx', 1, {}) YIELD node RETURN id(node)", vec_lit(&q)))
        .unwrap();
    assert_eq!(r.rows[0][0].to_string(), first);
    drop(tx);

    // Most vectors replaced: the graph is built again from the live ones and still finds them.
    let replaced: Vec<String> = (0..1200).map(|_| vec_lit(&point())).collect();
    run(
        &db,
        &format!(
            "MATCH (n:H) WITH n ORDER BY id(n) WITH collect(n)[..1200] AS ns, [{}] AS es UNWIND range(0, 1199) AS i WITH ns[i] AS n, es[i] AS e SET n.e = e",
            replaced.join(", ")
        ),
    );
    let queries: Vec<Vec<f64>> = (0..20).map(|_| point()).collect();
    let r = recall(&db, &queries, 10);
    assert!(r >= 0.9, "recall after updates {r}");
}
