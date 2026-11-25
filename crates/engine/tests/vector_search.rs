//! `MATCH … SEARCH v IN (VECTOR INDEX … FOR … [WHERE …] LIMIT k) [SCORE AS s]`, end to end
//! on every store: results against a scan, every place the clause can stand, and errors.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Database, KernelMode, QueryResult, StorageMode, Value};

type Db = Database<BoxedStore>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-vstest-{}-{}-{name}",
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

fn shown(r: QueryResult) -> Vec<String> {
    r.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).collect()
}

fn rows(db: &Db, q: &str) -> Vec<String> {
    shown(run(db, q))
}

fn error(db: &Db, q: &str) -> String {
    match db.execute(q) {
        Ok(r) => panic!("`{q}` gave {:?}", shown(r)),
        Err(e) => e.to_string(),
    }
}

/// Documents with a 2-dimensional vector each, all of them on a quarter circle: `k` grows
/// with the angle from `[1, 0]`.
fn docs(db: &Db, hnsw: bool) {
    run(
        db,
        &format!(
            "CREATE VECTOR INDEX docs FOR (d:Doc) ON d.e WITH [d.tenant, d.year, d.open] \
             OPTIONS {{indexConfig: {{`vector.dimensions`: 2, `vector.hnsw.enabled`: {hnsw}}}}}"
        ),
    );
    run(
        db,
        "UNWIND range(0, 89) AS k CREATE (d:Doc {k: k, e: [cos(radians(k)), sin(radians(k))], \
         tenant: 't' + toString(k % 3), year: 2000 + k % 10, open: k % 2 = 0}) \
         FOREACH (_ IN CASE WHEN k % 5 = 0 THEN [1] ELSE [] END | SET d:Fifth REMOVE d.tenant)",
    );
}

/// `MATCH (d:Doc) SEARCH … ` for `[1, 0]` with `filter`, giving `d.k`.
fn search(filter: &str, limit: usize) -> String {
    let filter = if filter.is_empty() { String::new() } else { format!("WHERE {filter} ") };
    format!("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] {filter}LIMIT {limit}) SCORE AS s RETURN d.k, s")
}

#[test]
fn filters_against_a_scan() {
    on_every_store(|store, db, reopen| {
        docs(&db, false);
        let db = reopen(db);
        let check = |run: &dyn Fn(&str) -> Vec<String>, what: &str| {
            for filter in [
                "",
                "d.tenant = 't1'",
                "'t1' = d.tenant",
                "d.tenant = null",
                "d.tenant = 5",
                "d.year > 2004",
                "2004 < d.year",
                "d.year >= 2004.5",
                "2003 <= d.year < 2006",
                "d.year > 2001 AND d.year <= 2008 AND d.tenant = 't2'",
                "d.tenant IN ['t0', 't2', null]",
                "d.year IN range(2003, 2005)",
                "d.tenant IN []",
                "d.tenant IN null",
                "d.open",
                "NOT d.open",
                "d.open = true AND (d.year < 2005 AND d.tenant = toLower('T0'))",
                "d.year = 2000 + 3 * 2",
            ] {
                for k in [1, 4, 1000] {
                    let w = if filter.is_empty() { String::new() } else { format!("WHERE {filter} ") };
                    let scan = format!(
                        "MATCH (d:Doc) {w}WITH d, vector.similarity.cosine(d.e, [1, 0]) AS s \
                         RETURN d.k, s ORDER BY s DESC, id(d) LIMIT {k}"
                    );
                    assert_eq!(run(&search(filter, k)), run(&scan), "{store}, {what}: {filter}, {k}");
                }
            }
        };
        check(&|q| rows(&db, q), "built");
        run(&db, "MATCH (d:Doc) WHERE d.k % 7 = 0 SET d.tenant = 't1', d.year = null");
        run(&db, "MATCH (d:Doc) WHERE d.k % 11 = 0 DETACH DELETE d");
        check(&|q| rows(&db, q), "after commits");
        let tx = std::cell::RefCell::new(db.begin().unwrap());
        let in_tx = |q: &str| shown(tx.borrow_mut().execute(q).unwrap_or_else(|e| panic!("{q}: {e}")));
        in_tx("MATCH (d:Doc) WHERE d.k % 4 = 1 SET d.tenant = 't2', d.open = NOT d.open");
        check(&in_tx, "in a transaction");
    });
}

#[test]
fn approximate_index() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    docs(&db, true);
    // So few that every filtered search is exact; the unfiltered one goes through the graph.
    assert_eq!(rows(&db, &search("d.tenant = 't1'", 2))[0], rows(&db, "MATCH (d {k: 1}) RETURN d.k, vector.similarity.cosine(d.e, [1, 0])")[0]);
    assert_eq!(rows(&db, &search("", 3)).len(), 3);
    assert_eq!(rows(&db, &search("d.year = 2007 AND d.open = false", 100)).len(), 9);
}

#[test]
fn the_match_filters_the_results() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    docs(&db, false);
    let ks = |q: &str| rows(&db, q);
    let found = "SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 10) SCORE AS s";
    // The WHERE before or after the SEARCH, labels and properties: all on the ten found.
    assert_eq!(ks(&format!("MATCH (d:Doc) {found} WHERE d.k > 6 RETURN d.k")), ["7", "8", "9"]);
    assert_eq!(ks(&format!("MATCH (d:Doc) WHERE d.k > 6 {found} RETURN d.k")), ["7", "8", "9"]);
    assert_eq!(ks(&format!("MATCH (d:Doc&Fifth) {found} RETURN d.k")), ["0", "5"]);
    assert_eq!(ks(&format!("MATCH (d:Doc {{year: 2003}}) {found} RETURN d.k")), ["3"]);
    assert_eq!(ks(&format!("MATCH (d) {found} RETURN count(*)")), ["10"]);
    assert_eq!(ks(&format!("MATCH (d:Nothing) {found} RETURN d")), [""; 0]);
    // The WHERE sees the score.
    assert_eq!(ks(&format!("MATCH (d:Doc) {found} WHERE s > 0.9999 RETURN d.k")), ["0", "1"]);
    assert_eq!(ks(&format!("MATCH (d:Doc) {found} WHERE s > 0.9999 AND d.k > 0 RETURN d.k")), ["1"]);
    // One hop from what was found, in its order.
    run(&db, "MATCH (a:Doc {k: 2}), (b:Doc {k: 8}) CREATE (a)-[:LINK]->(b), (a)-[:LINK]->(b), (b)-[:LINK]->(a)");
    assert_eq!(ks(&format!("MATCH (d:Doc)-->() {found} RETURN d.k")), ["2", "2", "8"]);
    assert_eq!(ks(&format!("MATCH ()-->(d:Doc) {found} RETURN d.k")), ["2", "8", "8"]);
    // Without a score, and with every column.
    assert_eq!(ks("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [0, 1] LIMIT 2) RETURN d.k"), ["89", "88"]);
    let r = run(&db, &format!("MATCH (d:Doc) {found} RETURN *"));
    assert_eq!(r.columns, ["d", "s"]);
    assert_eq!(r.rows.len(), 10);
}

#[test]
fn rows_parameters_and_subqueries() {
    on_every_store(|store, db, _| {
        docs(&db, false);
        let ks = |q: &str| rows(&db, q);
        // Per input row: the vector, the filter's values and the limit may use its variables.
        assert_eq!(
            ks("UNWIND [[11, 't1', 2], [51, 't2', 1]] AS p MATCH (o:Doc {k: p[0]}) \
                MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR o.e WHERE d.tenant = p[1] AND d.year > o.year LIMIT p[2]) SCORE AS s \
                RETURN o.k, d.k, s < 1 ORDER BY o.k, s DESC, d.k"),
            ["11, 13, true", "11, 7, true", "51, 53, true"],
            "{store}"
        );
        // Parameters everywhere.
        let params: BTreeMap<String, Value> = [
            ("q".to_string(), Value::List(vec![Value::Int(0), Value::Int(1)])),
            ("ts".to_string(), Value::List(vec![Value::String("t0".into()), Value::String("t1".into())])),
            ("k".to_string(), Value::Int(3)),
        ]
        .into();
        let q = "MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR $q WHERE d.tenant IN $ts LIMIT $k) RETURN d.k";
        assert_eq!(shown(db.execute_with_params(q, &params).unwrap()), ["88", "87", "84"], "{store}");
        // No rows, or one of nulls: for a null vector, and when nothing passes.
        for nothing in ["FOR null LIMIT 3", "FOR [1, 0] WHERE d.tenant = 'x' LIMIT 3", "FOR [1, 0] LIMIT 0"] {
            let s = format!("SEARCH d IN (VECTOR INDEX docs {nothing}) SCORE AS s");
            assert_eq!(ks(&format!("UNWIND [1, 2] AS x MATCH (d:Doc) {s} RETURN x")), [""; 0], "{store}: {nothing}");
            assert_eq!(
                ks(&format!("UNWIND [1, 2] AS x OPTIONAL MATCH (d:Doc) {s} RETURN x, d, s")),
                ["1, null, null", "2, null, null"],
                "{store}: {nothing}"
            );
        }
        assert_eq!(
            ks("UNWIND [1, 70] AS y OPTIONAL MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 2) WHERE d.k < y RETURN y, d.k"),
            ["1, 0", "70, 0", "70, 1"],
            "{store}"
        );
        // In subqueries and unions.
        let one = "MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR v WHERE d.open LIMIT 1)";
        assert_eq!(ks(&format!("UNWIND [[1, 0], [0, 1]] AS v CALL (v) {{ {one} RETURN d.k AS k }} RETURN k")), ["0", "88"], "{store}");
        assert_eq!(ks(&format!("UNWIND [[1, 0], [0, 1]] AS v RETURN COUNT {{ {one} WHERE d.k > 5 }}")), ["0", "1"], "{store}");
        assert_eq!(
            ks(&format!("WITH [1, 0] AS v {one} RETURN d.k AS k UNION ALL WITH [0, 1] AS v {one} RETURN d.k AS k")),
            ["0", "88"],
            "{store}"
        );
        // Found entities can be changed, and a query sees what it wrote.
        run(&db, "MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] WHERE d.open LIMIT 2) SET d.open = false");
        assert_eq!(ks(&search("d.open", 1))[0].split(',').next(), Some("4"), "{store}");
        assert_eq!(
            ks("CREATE (n:Doc {k: 1000, e: [1, 0], open: true}) WITH n MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] WHERE d.open LIMIT 1) RETURN d.k"),
            ["1000"],
            "{store}"
        );
        // The limits further up stop it early without changing anything.
        assert_eq!(ks("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [0, 1] LIMIT 50) RETURN d.k LIMIT 2"), ["89", "88"], "{store}");
    });
}

#[test]
fn relationship_index() {
    on_every_store(|store, db, reopen| {
        run(
            &db,
            "CREATE VECTOR INDEX near FOR ()-[r:NEAR]-() ON (r.v) WITH [r.since] OPTIONS {indexConfig: {`vector.dimensions`: 2}}",
        );
        run(
            &db,
            "CREATE (a:A)-[:NEAR {v: [1, 0], since: 1, t: 'x'}]->(b:B), (b)-[:NEAR {v: [1, 1], since: 2, t: 'y'}]->(a), \
             (a)-[:NEAR {v: [0, 1], t: 'z'}]->(a), (a)-[:FAR {v: [1, 0], since: 1, t: 'far'}]->(b)",
        );
        let db = reopen(db);
        let found = |pattern: &str, inner: &str| {
            rows(&db, &format!("MATCH {pattern} SEARCH r IN (VECTOR INDEX near FOR [1, 0] {inner}) SCORE AS s RETURN r.t"))
        };
        for pattern in ["()-[r:NEAR]->()", "()<-[r:NEAR]-()", "()-[r]->()"] {
            assert_eq!(found(pattern, "LIMIT 5"), ["\"x\"", "\"y\"", "\"z\""], "{store}: {pattern}");
        }
        assert_eq!(found("()-[r:NEAR]->()", "WHERE r.since >= 2 LIMIT 5"), ["\"y\""], "{store}");
        // What the pattern says filters the results.
        assert_eq!(found("()-[r:NEAR {t: 'y'}]->()", "LIMIT 5"), ["\"y\""], "{store}");
        assert_eq!(found("()-[r:NEAR {t: 'y'}]->()", "LIMIT 1"), [""; 0], "{store}");
        assert_eq!(found("()-[r:FAR]->()", "LIMIT 5"), [""; 0], "{store}");
        assert_eq!(found("()-[r:!NEAR]->()", "LIMIT 5"), [""; 0], "{store}");
        assert_eq!(
            rows(&db, "MATCH ()-[r]->() SEARCH r IN (VECTOR INDEX near FOR [1, 0] LIMIT 1) RETURN labels(startNode(r)), type(r)"),
            ["[\"A\"], \"NEAR\""],
            "{store}"
        );
        // In a transaction.
        let mut tx = db.begin().unwrap();
        tx.execute("MATCH ()-[r:NEAR {t: 'z'}]->() SET r.since = 9").unwrap();
        let r = tx
            .execute("MATCH ()-[r:NEAR]->() SEARCH r IN (VECTOR INDEX near FOR [1, 0] WHERE r.since > 2 LIMIT 5) RETURN r.t")
            .unwrap();
        assert_eq!(shown(r), ["\"z\""], "{store}");
    });
}

/// Queries of shapes the kernels answer without a `SEARCH`.
#[test]
fn kernels_leave_searches_alone() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    docs(&db, false);
    db.set_kernel_mode(KernelMode::Always);
    for (search, want) in [("", "90"), ("SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 10) ", "10")] {
        for returns in ["count(*)", "count(d)"] {
            let q = format!("MATCH (d:Doc) {search}RETURN {returns}");
            assert_eq!(rows(&db, &q), [want], "{q}");
            assert_eq!(db.explain(&q).unwrap().contains("Kernel"), search.is_empty(), "{q}");
        }
    }
    let q = "MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 3) RETURN d.k ORDER BY d.k DESC LIMIT 1";
    assert_eq!(rows(&db, q), ["2"]);
    run(&db, "MATCH (a:Doc {k: 2}), (b:Doc {k: 80}) CREATE (a)-[:LINK]->(b), (b)-[:LINK]->(a)");
    assert_eq!(rows(&db, "MATCH (d:Doc)-->() SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 3) RETURN count(*)"), ["1"]);
}

#[test]
fn plans() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    docs(&db, false);
    let q = "MATCH (d:Doc:Fifth) SEARCH d IN (VECTOR INDEX docs FOR $q WHERE d.year >= 2001 AND d.tenant IN $ts LIMIT 7) SCORE AS s RETURN d.k";
    let params: BTreeMap<String, Value> = [
        ("q".to_string(), Value::List(vec![Value::Int(1), Value::Int(0)])),
        ("ts".to_string(), Value::List(vec![Value::String("t1".into())])),
    ]
    .into();
    for mode in ["EXPLAIN", "PROFILE"] {
        let r = db.execute_with_params(&format!("{mode} {q}"), &params).unwrap();
        let plan = r.plan.expect("a plan").to_string();
        assert!(
            plan.contains("VectorSearch (d) on docs WHERE d.year >= 2001 AND d.tenant IN $ts LIMIT 7 SCORE AS s"),
            "{mode}: {plan}"
        );
        assert!(plan.contains("Argument (d:Doc:Fifth)"), "{mode}: {plan}");
    }
}

/// `PROFILE` says how the results were found; `EXPLAIN` does not know.
#[test]
fn profiled_searches() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    for (name, label, hnsw) in [("flat", "F", false), ("graph", "G", true)] {
        run(
            &db,
            &format!(
                "CREATE VECTOR INDEX {name} FOR (d:{label}) ON d.e WITH [d.p]                  OPTIONS {{indexConfig: {{`vector.dimensions`: 2, `vector.hnsw.enabled`: {hnsw}}}}}"
            ),
        );
    }
    run(&db, "UNWIND range(1, 2000) AS i CREATE (:F:G {p: i, e: [sin(i), cos(i)]})");
    let line = |plan: String| {
        plan.lines().find(|l| l.contains("VectorSearch")).unwrap_or_else(|| panic!("{plan}")).trim().to_string()
    };
    let profile = |q: &str| line(run(&db, &format!("PROFILE {q}")).plan.expect("a plan").to_string());
    let search = |index: &str, inner: &str| {
        format!("MATCH (d) SEARCH d IN (VECTOR INDEX {index} FOR [1, 0] {inner}) SCORE AS s RETURN d.p")
    };
    for (index, inner, how) in [
        ("flat", "LIMIT 3", "all vectors"),
        ("flat", "WHERE d.p <= 10 LIMIT 3", "matching vectors"),
        ("flat", "WHERE d.p <= 1900 LIMIT 3", "all vectors"),
        ("flat", "WHERE d.p = 0 LIMIT 3", "matching vectors"),
        ("graph", "LIMIT 3", "HNSW graph"),
        ("graph", "WHERE d.p <= 10 LIMIT 3", "matching vectors"),
        ("graph", "WHERE d.p <= 1990 LIMIT 3", "HNSW graph"),
    ] {
        let text = profile(&search(index, inner));
        assert!(text.contains(&format!("SCORE AS s by {how} (~")), "{index} {inner}: {text}");
        let explained = line(db.explain(&search(index, inner)).unwrap());
        assert!(!explained.contains(" by "), "{index} {inner}: {explained}");
    }
    // Nothing searched.
    assert!(!profile(&search("flat", "LIMIT 0")).contains(" by "));
    assert!(!profile("MATCH (d) SEARCH d IN (VECTOR INDEX flat FOR null LIMIT 3) RETURN d").contains(" by "));
    // Several searches of one step.
    let text = profile("UNWIND [10, 20, 1900] AS x MATCH (d) SEARCH d IN (VECTOR INDEX flat FOR [1, 0] WHERE d.p <= x LIMIT 3) RETURN d");
    assert!(text.contains(" by matching vectors (2), all vectors (1) (~"), "{text}");
    let text = profile("UNWIND [10, 20] AS x MATCH (d) SEARCH d IN (VECTOR INDEX flat FOR [1, 0] WHERE d.p <= x LIMIT 3) RETURN d");
    assert!(text.contains(" by matching vectors (~"), "{text}");
    // Inside a transaction.
    let mut tx = db.begin().unwrap();
    let r = tx.execute(&format!("PROFILE {}", search("graph", "LIMIT 3"))).unwrap();
    let text = line(r.plan.expect("a plan").to_string());
    assert!(text.contains(" by scan of the store"), "{text}");
    drop(tx);
    // Other lines say nothing of it.
    let plan = run(&db, &format!("PROFILE {}", search("flat", "LIMIT 3"))).plan.expect("a plan").to_string();
    assert_eq!(plan.matches(" by ").count(), 1, "{plan}");
}

#[test]
fn errors() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    docs(&db, false);
    run(&db, "CREATE VECTOR INDEX near FOR ()-[r:NEAR]-() ON (r.v) OPTIONS {indexConfig: {`vector.dimensions`: 2}}");
    run(&db, "CREATE FULLTEXT INDEX words FOR (d:Doc) ON EACH [d.text]");
    let s = "SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 3)";
    let w = |w: &str| format!("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] WHERE {w} LIMIT 3) RETURN d");
    let other = "other conditions go in the WHERE of the MATCH";
    for (q, want) in [
        // The index.
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX nope FOR [1, 0] LIMIT 3) RETURN d".into(), "no vector index named `nope`"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX words FOR [1, 0] LIMIT 3) RETURN d".into(), "no vector index named `words`"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX near FOR [1, 0] LIMIT 3) RETURN d".into(), "indexes relationships, and `d` is a node"),
        ("MATCH ()-[d]->() SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 3) RETURN d".into(), "indexes nodes, and `d` is a relationship"),
        // The pattern.
        (format!("MATCH (d:Doc), (x) {s} RETURN d"), "a single pattern"),
        (format!("MATCH p = (d:Doc) {s} RETURN d"), "cannot name its path"),
        (format!("MATCH shortestPath((d)-[*]->()) {s} RETURN d"), "cannot search for paths"),
        (format!("MATCH (d:Doc)-->()-->() {s} RETURN d"), "at most one relationship"),
        (format!("MATCH (d:Doc)-[*1..2]->() {s} RETURN d"), "of a fixed length"),
        (format!("MATCH (d:Doc)-[x]->() {s} RETURN d"), "can name only `d`"),
        (format!("MATCH (d:Doc)-->(x) {s} RETURN d"), "can name only `d`"),
        (format!("MATCH (d)-->(d) {s} RETURN d"), "can name only `d`"),
        (format!("MATCH (d:Doc)-[:LINK]->() {s} RETURN d"), "only on `d`"),
        (format!("MATCH (d:Doc)-->(:Doc) {s} RETURN d"), "only on `d`"),
        (format!("MATCH (d:Doc)-->({{k: 1}}) {s} RETURN d"), "only on `d`"),
        (format!("MATCH (x:Doc) {s} RETURN x"), "can name only `d`"),
        (format!("MATCH (:Doc) {s} RETURN 1"), "only on `d`"),
        (format!("MATCH () {s} RETURN 1"), "`d` is not in the pattern"),
        ("MATCH ()-[r]-() SEARCH r IN (VECTOR INDEX near FOR [1, 0] LIMIT 3) RETURN r".into(), "needs a direction"),
        // The variables.
        (format!("MATCH (d) WITH d MATCH (d:Doc) {s} RETURN d"), "variable `d` already declared"),
        (format!("MATCH (d:Doc) {s} SCORE AS d RETURN d"), "variable `d` already declared"),
        (format!("WITH 1 AS x MATCH (d:Doc) {s} SCORE AS x RETURN d"), "variable `x` already declared"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR d.e LIMIT 3) RETURN d".into(), "the query vector cannot use `d`"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR x.e LIMIT 3) RETURN d".into(), "`x` not defined"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT d.k) RETURN d".into(), "LIMIT cannot use `d`"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT s) SCORE AS s RETURN d".into(), "`s` not defined"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT count(*)) RETURN d".into(), "aggregate"),
        (w("d.year = d.k"), "a value in WHERE cannot use `d`"),
        (w("d.year > size(d.tenant)"), "a value in WHERE cannot use `d`"),
        (w("d.year = y"), "`y` not defined"),
        // The filter.
        (w("d.k = 1"), "`k` is not a filter property of vector index `docs`"),
        (w("d.k"), "`k` is not a filter property"),
        (w("d.e = [1, 0]"), "`e` is not a filter property"),
        (w("d.year <> 2001"), other),
        (w("d.year = 2001 OR d.open"), other),
        (w("NOT (d.year = 2001)"), other),
        (w("d.tenant STARTS WITH 't'"), other),
        (w("d.tenant IS NULL"), other),
        (w("d.tenant IS NOT NULL AND d.open"), other),
        (w("d.year + 1 = 2002"), other),
        (w("d:Fifth"), other),
        (w("2001 IN d.year"), other),
        (w("true"), other),
        (w("(d)-->()"), other),
        // The values, when it runs.
        (w("d.year < [2001]"), "not with a LIST"),
        (w("d.year = {a: 1}"), "not with a MAP"),
        (w("d.year = point({x: 1, y: 2})"), "not with a POINT"),
        (w("d.year IN [1, [2]]"), "not with a LIST"),
        (w("d.year IN 2001"), "IN needs a list"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT -1) RETURN d".into(), "LIMIT must be from 0"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 2147483648) RETURN d".into(), "LIMIT must be from 0"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT 1.5) RETURN d".into(), "must be an INTEGER"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] LIMIT null) RETURN d".into(), "must be an INTEGER"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1] LIMIT 3) RETURN d".into(), "has 2 dimensions, the query vector 1"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [0, 0] LIMIT 3) RETURN d".into(), "zero vector"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR ['x', 0] LIMIT 3) RETURN d".into(), "INTEGER or FLOAT"),
        ("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR 'x' LIMIT 3) RETURN d".into(), "as a vector"),
    ] {
        let err = error(&db, &q);
        assert!(err.contains(want), "{q}\n  {err}\n  not: {want}");
    }
}
