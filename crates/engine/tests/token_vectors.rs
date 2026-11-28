//! Sparse (lexical weights) and multi-vector (ColBERT) indexes, end to end: their
//! definitions on every store, and their agreement with `vector.sparseScore` /
//! `vector.maxSim` under `ORDER BY … LIMIT k`.

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
            "graphbuff-ttest-{}-{}-{name}",
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
    run(db, q).rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).collect()
}

fn error(db: &Db, q: &str) -> String {
    match db.execute(q) {
        Ok(_) => panic!("`{q}` succeeded"),
        Err(e) => e.to_string(),
    }
}

/// A deterministic stream of numbers in [-1, 1).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 40) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0
    }

    fn below(&mut self, n: usize) -> usize {
        (((self.next() + 1.0) / 2.0) * n as f64) as usize % n
    }
}

const SPARSE: &str = "CREATE VECTOR INDEX lex FOR (d:D) ON (d.t, d.w) WITH [d.g] OPTIONS {indexConfig: {`vector.type`: 'sparse'}}";

/// The properties of a document of lexical weights: distinct tokens of a small vocabulary,
/// coarse weights (so that scores tie), some 0 and some negative.
fn sparse_doc(r: &mut Lcg, i: usize) -> String {
    let n = 1 + r.below(6);
    let mut tokens: Vec<usize> = (0..n).map(|_| r.below(40)).collect();
    tokens.sort_unstable();
    tokens.dedup();
    let t: Vec<String> = tokens.iter().map(|t| format!("'t{t}'")).collect();
    let w: Vec<String> = tokens.iter().map(|_| format!("{}", (r.next() * 4.0).round() / 4.0 + 0.25)).collect();
    format!("{{k: {i}, t: [{}], w: [{}], g: {}}}", t.join(", "), w.join(", "), i % 3)
}

fn sparse_query(r: &mut Lcg) -> String {
    let n = 1 + r.below(5);
    let parts: Vec<String> = (0..n).map(|_| format!("t{}: {}", r.below(45), (r.next() * 3.0).round() / 2.0)).collect();
    format!("{{{}}}", parts.join(", "))
}

/// What a sparse search must give: the entities sharing a token with `q` (neither weight
/// 0), by `vector.sparseScore`.
fn sparse_scan(q: &str, k: usize, filter: &str) -> String {
    format!(
        "WITH {q} AS q MATCH (d:D) WHERE any(i IN range(0, size(d.t) - 1) WHERE d.w[i] <> 0 AND q[d.t[i]] <> 0) {filter} \
         WITH d, vector.sparseScore(q, d.t, d.w) AS score RETURN d.k, score ORDER BY score DESC, id(d) LIMIT {k}"
    )
}

fn sparse_search(q: &str, k: usize) -> String {
    format!("CALL db.index.vector.queryNodes('lex', {k}, {q}) YIELD node, score RETURN node.k, score")
}

fn check_sparse(db: &Db, r: &mut Lcg, what: &str) {
    for _ in 0..12 {
        let q = sparse_query(r);
        for k in [1, 5, 40, 1000] {
            assert_eq!(rows(db, &sparse_search(&q, k)), rows(db, &sparse_scan(&q, k, "")), "{what}: {q}, k = {k}");
        }
        let search = format!(
            "MATCH (d:D) SEARCH d IN (VECTOR INDEX lex FOR {q} WHERE d.g = 1 LIMIT 7) SCORE AS score RETURN d.k, score"
        );
        assert_eq!(rows(db, &search), rows(db, &sparse_scan(&q, 7, "AND d.g = 1")), "{what}, filtered: {q}");
    }
}

#[test]
fn sparse_like_order_by() {
    on_every_store(|store, db, reopen| {
        run(&db, SPARSE);
        let mut r = Lcg(5);
        let docs: Vec<String> = (0..600).map(|i| sparse_doc(&mut r, i)).collect();
        run(&db, &format!("UNWIND [{}] AS p CREATE (d:D) SET d = p", docs.join(", ")));
        // Values the index skips.
        run(&db, "CREATE (:E {t: ['t1'], w: [1]}), (:D {k: -1, t: ['t1'], w: [1, 2]}), (:D {k: -2, t: 't1', w: 1})");
        run(&db, "CREATE (:D {k: -3, t: ['t1', 2], w: [1, 1]}), (:D {k: -4, t: ['t1']})");
        let found = rows(&db, &sparse_search("{t1: 1}", 1000));
        assert!(!found.is_empty() && found.iter().all(|r| !r.starts_with('-')), "{store}: {found:?}");
        // (The scan below cannot read them.)
        run(&db, "MATCH (d) WHERE d.k < 0 OR d:E DETACH DELETE d");
        let mut r = Lcg(11);
        check_sparse(&db, &mut r, &format!("{store}: built"));
        // Commits: changed weights, tokens, filter values, entities gone and new.
        run(&db, "MATCH (d:D) WHERE d.k % 5 = 0 SET d.w = [x IN d.w | x * 2 - 0.5]");
        run(&db, "MATCH (d:D) WHERE d.k % 7 = 0 SET d.t = d.t[1..] + ['t44'], d.w = d.w[1..] + [3]");
        run(&db, "MATCH (d:D) WHERE d.k % 11 = 0 SET d.g = 1");
        run(&db, "MATCH (d:D) WHERE d.k % 13 = 0 DETACH DELETE d");
        let more: Vec<String> = (600..700).map(|i| sparse_doc(&mut r, i)).collect();
        run(&db, &format!("UNWIND [{}] AS p CREATE (d:D) SET d = p", more.join(", ")));
        check_sparse(&db, &mut r, &format!("{store}: after commits"));
        // Most replaced: the slots are numbered anew.
        run(&db, "MATCH (d:D) WHERE d.k % 4 <> 0 SET d.w = [x IN d.w | -x]");
        check_sparse(&db, &mut r, &format!("{store}: most replaced"));
        let db = reopen(db);
        check_sparse(&db, &mut r, &format!("{store}: reopened"));
        // Inside a transaction, its own writes count.
        let mut tx = db.begin().unwrap();
        tx.execute("MATCH (d:D) WHERE d.k % 3 = 0 SET d.w = [x IN d.w | x + 1]").unwrap();
        for _ in 0..5 {
            let q = sparse_query(&mut r);
            let shown = |res: QueryResult| -> Vec<String> {
                res.rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).collect()
            };
            let (a, b) = (tx.execute(&sparse_search(&q, 10)).unwrap(), tx.execute(&sparse_scan(&q, 10, "")).unwrap());
            assert_eq!(shown(a), shown(b), "{store}: in a transaction: {q}");
        }
    });
}

#[test]
fn sparse_details() {
    let db = mem();
    run(&db, SPARSE);
    run(&db, "CREATE (:D {k: 1, t: ['a', 'b', 'a'], w: [1, 2, 3]}), (:D {k: 2, t: ['b'], w: [0]}), (:D {k: 3, t: ['c'], w: [5]})");
    // A repeated token takes its last weight, as vector.sparseScore reads it.
    assert_eq!(rows(&db, &sparse_search("{a: 1.0}", 5)), ["1, 3.0"]);
    // Only entities sharing a token (both weights not 0) are found; null finds nothing.
    assert_eq!(rows(&db, &sparse_search("{b: 1, z: 2}", 5)), ["1, 2.0"]);
    assert_eq!(rows(&db, &sparse_search("{c: 0}", 5)), Vec::<String>::new());
    assert_eq!(rows(&db, "MATCH (d:D) SEARCH d IN (VECTOR INDEX lex FOR null LIMIT 3) RETURN d.k"), Vec::<String>::new());
    // Relationships.
    run(&db, "CREATE VECTOR INDEX rlex FOR ()-[r:R]-() ON (r.t, r.w) OPTIONS {indexConfig: {`vector.type`: 'sparse'}}");
    run(&db, "CREATE ()-[:R {t: ['x', 'y'], w: [1, 1]}]->(), ()-[:R {t: ['y'], w: [3]}]->()");
    assert_eq!(
        rows(&db, "CALL db.index.vector.queryRelationships('rlex', 2, {y: 1, x: 1}) YIELD relationship, score RETURN relationship.w, score"),
        ["[3], 3.0", "[1, 1], 2.0"]
    );
    // PROFILE says how.
    let plan = run(&db, "PROFILE MATCH (d:D) SEARCH d IN (VECTOR INDEX lex FOR {a: 1} LIMIT 3) RETURN d").plan.expect("a plan").to_string();
    assert!(plan.contains("by inverted index"), "{plan}");
}

const DIMS: usize = 4;

/// The vectors of a document, one after another: 1 to 5 of coarse coordinates.
fn multi_doc(r: &mut Lcg, i: usize) -> String {
    let n = 1 + r.below(5);
    let v: Vec<String> = (0..n * DIMS).map(|_| format!("{}", (r.next() * 4.0).round() / 4.0)).collect();
    format!("{{k: {i}, v: [{}], g: {}}}", v.join(", "), i % 3)
}

fn multi_query(r: &mut Lcg) -> String {
    let n = 1 + r.below(4);
    let vs: Vec<String> = (0..n)
        .map(|_| format!("[{}]", (0..DIMS).map(|_| format!("{:.3}", r.next())).collect::<Vec<_>>().join(", ")))
        .collect();
    format!("[{}]", vs.join(", "))
}

fn multi_scan(q: &str, k: usize, filter: &str) -> String {
    format!(
        "MATCH (d:D) WHERE d.v IS NOT NULL {filter} WITH d, vector.maxSim({q}, d.v) AS score \
         RETURN d.k, score ORDER BY score DESC, id(d) LIMIT {k}"
    )
}

fn multi_search(index: &str, q: &str, k: usize) -> String {
    format!("CALL db.index.vector.queryNodes('{index}', {k}, {q}) YIELD node, score RETURN node.k, score")
}

fn check_multi(db: &Db, r: &mut Lcg, what: &str) {
    for _ in 0..12 {
        let q = multi_query(r);
        for k in [1, 5, 40, 1000] {
            assert_eq!(rows(db, &multi_search("col", &q, k)), rows(db, &multi_scan(&q, k, "")), "{what}: {q}, k = {k}");
        }
        for (filter, cond) in [("d.g = 1", "AND d.g = 1"), ("d.k < 30", "AND d.k < 30")] {
            let search = format!(
                "MATCH (d:D) SEARCH d IN (VECTOR INDEX col FOR {q} WHERE {filter} LIMIT 7) SCORE AS score RETURN d.k, score"
            );
            assert_eq!(rows(db, &search), rows(db, &multi_scan(&q, 7, cond)), "{what}, {filter}: {q}");
        }
    }
}

#[test]
fn multi_like_order_by() {
    on_every_store(|store, db, reopen| {
        run(&db, &format!(
            "CREATE VECTOR INDEX col FOR (d:D) ON d.v WITH [d.g, d.k] OPTIONS {{indexConfig: {{`vector.type`: 'multi', `vector.dimensions`: {DIMS}}}}}"
        ));
        let mut r = Lcg(3);
        let docs: Vec<String> = (0..500).map(|i| multi_doc(&mut r, i)).collect();
        run(&db, &format!("UNWIND [{}] AS p CREATE (d:D) SET d = p", docs.join(", ")));
        // Skipped: not of whole vectors, not numbers, another label.
        run(&db, "CREATE (:E {v: [1, 0, 0, 0]}), (:D {k: -1, v: [1, 2, 3]}), (:D {k: -2, v: ['a', 'b', 'c', 'd']})");
        run(&db, "MATCH (d:D) WHERE d.k < 0 SET d.v = null");
        check_multi(&db, &mut r, &format!("{store}: built"));
        run(&db, "MATCH (d:D) WHERE d.k % 5 = 0 SET d.v = d.v + [1, 1, 0, 0]");
        run(&db, "MATCH (d:D) WHERE d.k % 7 = 0 SET d.v = d.v[0..4]");
        run(&db, "MATCH (d:D) WHERE d.k % 11 = 0 SET d.g = 1");
        run(&db, "MATCH (d:D) WHERE d.k % 13 = 0 DETACH DELETE d");
        let more: Vec<String> = (500..600).map(|i| multi_doc(&mut r, i)).collect();
        run(&db, &format!("UNWIND [{}] AS p CREATE (d:D) SET d = p", more.join(", ")));
        check_multi(&db, &mut r, &format!("{store}: after commits"));
        run(&db, "MATCH (d:D) WHERE d.k % 4 <> 0 SET d.v = [x IN d.v | -x]");
        check_multi(&db, &mut r, &format!("{store}: most replaced"));
        let db = reopen(db);
        check_multi(&db, &mut r, &format!("{store}: reopened"));
        let mut tx = db.begin().unwrap();
        tx.execute("MATCH (d:D) WHERE d.k % 3 = 0 SET d.v = [x IN d.v | x + 0.5]").unwrap();
        for _ in 0..5 {
            let q = multi_query(&mut r);
            let shown = |res: QueryResult| -> Vec<String> {
                res.rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).collect()
            };
            let (a, b) = (tx.execute(&multi_search("col", &q, 10)).unwrap(), tx.execute(&multi_scan(&q, 10, "")).unwrap());
            assert_eq!(shown(a), shown(b), "{store}: in a transaction: {q}");
        }
    });
}

#[test]
fn approximate_multi_vector() {
    let db = mem();
    let dims = 16;
    run(&db, &format!(
        "CREATE VECTOR INDEX col FOR (d:D) ON d.v WITH [d.g] OPTIONS {{indexConfig: {{`vector.type`: 'multi', \
         `vector.dimensions`: {dims}, `vector.hnsw.enabled`: true}}}}"
    ));
    // Documents of 3 to 8 unit vectors near one of 50 topics.
    let mut r = Lcg(17);
    let topics: Vec<Vec<f64>> = (0..50).map(|_| (0..dims).map(|_| r.next()).collect()).collect();
    let unit = |c: usize, r: &mut Lcg| -> Vec<f64> {
        let v: Vec<f64> = topics[c].iter().map(|x| x + 0.4 * r.next()).collect();
        let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        v.iter().map(|x| (x / n * 1e4).round() / 1e4).collect()
    };
    let docs: Vec<String> = (0..1500)
        .map(|i| {
            let n = 3 + r.below(6);
            let v: Vec<String> = (0..n).flat_map(|j| unit((i + j * 7) % 50, &mut r)).map(|x| x.to_string()).collect();
            format!("{{k: {i}, g: {}, v: [{}]}}", i % 4, v.join(", "))
        })
        .collect();
    run(&db, &format!("UNWIND [{}] AS p CREATE (d:D) SET d = p", docs.join(", ")));
    let (mut hits, mut total) = (0, 0);
    for i in 0..20 {
        let q: Vec<String> = (0..4).map(|j| {
            let v = unit((i * 3 + j) % 50, &mut r);
            format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "))
        }).collect();
        let q = format!("[{}]", q.join(", "));
        for (search, scan) in [
            (multi_search("col", &q, 10), multi_scan(&q, 10, "")),
            (
                format!("MATCH (d:D) SEARCH d IN (VECTOR INDEX col FOR {q} WHERE d.g = 2 LIMIT 10) SCORE AS s RETURN d.k, s"),
                multi_scan(&q, 10, "AND d.g = 2"),
            ),
        ] {
            let (got, want) = (rows(&db, &search), rows(&db, &scan));
            assert_eq!(got.len(), want.len());
            total += want.len();
            // Entity and score: the scores are exact.
            hits += got.iter().filter(|g| want.contains(g)).count();
        }
    }
    assert!(hits as f64 >= 0.9 * total as f64, "recall: {hits} of {total}");
    let plan = run(&db, &format!("PROFILE MATCH (d:D) SEARCH d IN (VECTOR INDEX col FOR [[{}]] LIMIT 3) RETURN d", vec!["0.25"; dims].join(", ")))
        .plan.expect("a plan").to_string();
    assert!(plan.contains("by HNSW graph"), "{plan}");
}

#[test]
fn definitions_on_every_store() {
    on_every_store(|store, db, reopen| {
        run(&db, SPARSE);
        run(&db, "CREATE VECTOR INDEX col FOR (d:D) ON d.c OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 2, `vector.embedding_model`: 'bge-m3'}}");
        run(&db, "CREATE VECTOR INDEX hcol FOR (d:H) ON d.c OPTIONS {indexConfig: {`vector.type`: 'MULTI', `vector.dimensions`: 2, `vector.hnsw.m`: 8}}");
        run(&db, "CREATE VECTOR INDEX dense FOR (d:D) ON d.c OPTIONS {indexConfig: {`vector.type`: 'dense', `vector.dimensions`: 2}}");
        run(&db, "CREATE (:D {k: 1, t: ['a'], w: [1], c: [1, 0, 0, 1]}), (:H {c: [0.5, 0.5]})");
        let show = |db: &Db| {
            let mut all: Vec<String> = rows(db, "SHOW INDEXES").into_iter().filter(|r| r.contains("\"VECTOR\"")).collect();
            all.sort();
            all
        };
        let bare = |s: &str| s.replace(['`', '\'', '"', ' '], "");
        let want = [
            "col, [c], {indexConfig: {`vector.dimensions`: 2, `vector.embedding_model`: 'bge-m3', `vector.hnsw.enabled`: false, `vector.type`: 'multi'}}",
            "dense, [c], {indexConfig: {`vector.dimensions`: 2, `vector.hnsw.enabled`: false, `vector.similarity_function`: 'COSINE'}}",
            "hcol, [c], {indexConfig: {`vector.dimensions`: 2, `vector.hnsw.ef_construction`: 100, `vector.hnsw.enabled`: true, `vector.hnsw.m`: 8, `vector.type`: 'multi'}}",
            "lex, [t, w, g], {indexConfig: {`vector.type`: 'sparse'}}",
        ];
        let got = show(&db);
        assert_eq!(got.len(), 4, "{store}: {got:?}");
        let db = reopen(db);
        assert_eq!(show(&db), got, "{store}: reopened");
        for (g, w) in got.iter().zip(want) {
            let (name, rest) = w.split_once(", ").unwrap();
            let (props, options) = rest.split_once("], ").unwrap();
            let g = bare(g);
            assert!(g.starts_with(&format!("{name},")) && g.contains(&bare(props)) && g.ends_with(&bare(options)), "{store}: {g}\nwant {w}");
        }
        assert_eq!(rows(&db, &sparse_search("{a: 2}", 3)), ["1, 2.0"], "{store}");
        assert_eq!(rows(&db, &multi_search("col", "[[0, 1]]", 3)), ["1, 1.0"], "{store}");
        assert_eq!(rows(&db, "CALL db.index.vector.queryNodes('hcol', 3, [[1, 1]]) YIELD score RETURN score"), ["1.0"], "{store}");
        // A dense and a multi-vector index sit on one property (`dense` and `col`), but not
        // two of one form.
        for q in [
            "CREATE VECTOR INDEX x FOR (d:D) ON d.c OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 4}}",
            "CREATE VECTOR INDEX x FOR (d:D) ON (d.t, d.w) OPTIONS {indexConfig: {`vector.type`: 'sparse'}}",
        ] {
            assert!(error(&db, q).contains("already exists"), "{store}: {q}");
        }
        run(&db, "DROP INDEX lex");
        let db = reopen(db);
        assert_eq!(show(&db).len(), 3, "{store}");
    });
}

#[test]
fn bad_definitions_and_queries() {
    let db = mem();
    for (q, want) in [
        ("CREATE VECTOR INDEX x FOR (d:D) ON d.t OPTIONS {indexConfig: {`vector.type`: 'sparse'}}", "two properties"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON (d.t, d.w, d.z) OPTIONS {indexConfig: {`vector.type`: 'sparse'}}", "two properties"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON (d.t, d.w) OPTIONS {indexConfig: {`vector.type`: 'sparse', `vector.dimensions`: 3}}", "takes no"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON (d.t, d.w) OPTIONS {indexConfig: {`vector.type`: 'sparse', `vector.hnsw.enabled`: true}}", "takes no"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON d.v OPTIONS {indexConfig: {`vector.type`: 'multi'}}", "vector.dimensions"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON d.v OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 2, `vector.similarity_function`: 'COSINE'}}", "MaxSim"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON (d.v, d.u) OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 2}}", "exactly one property"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON d.v OPTIONS {indexConfig: {`vector.type`: 'colbert', `vector.dimensions`: 2}}", "'dense', 'sparse' or 'multi'"),
        ("CREATE VECTOR INDEX x FOR (d:D) ON d.v OPTIONS {indexConfig: {`vector.type`: 3, `vector.dimensions`: 2}}", "STRING"),
    ] {
        let e = error(&db, q);
        assert!(e.contains(want), "{q}: {e}");
    }
    run(&db, SPARSE);
    run(&db, "CREATE VECTOR INDEX col FOR (d:D) ON d.v OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 2}}");
    for (q, want) in [
        (sparse_search("[1, 2]", 3), "MAP of token to weight"),
        (sparse_search("{a: 'x'}", 3), "finite numbers"),
        (multi_search("col", "{a: 1}", 3), "LIST<LIST<FLOAT>>"),
        (multi_search("col", "[[1, 2, 3]]", 3), "2 dimensions"),
        (multi_search("col", "[1, 2, 3]", 3), "not vectors of them"),
        (multi_search("col", "[]", 3), "at least one vector"),
    ] {
        let e = error(&db, &q);
        assert!(e.contains(want), "{q}: {e}");
    }
    // A flat query is taken as its vectors.
    run(&db, "CREATE (:D {k: 1, v: [1, 0, 0, 1]})");
    assert_eq!(rows(&db, &multi_search("col", "[1, 0, 0, 1]", 1)), ["1, 1.0"]);
}
