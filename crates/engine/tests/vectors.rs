//! Vectors, end to end: `vector.similarity.cosine` / `euclidean`, vector indexes
//! (`CREATE VECTOR INDEX`, `db.index.vector.queryNodes` / `queryRelationships`) on every
//! store, and their agreement with `ORDER BY similarity LIMIT k`.

use graphbuff::{BoxedStore, Database, Error, QueryResult, StorageMode};

type Db = Database<BoxedStore>;

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
