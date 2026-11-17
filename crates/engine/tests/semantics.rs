//! Numeric literal forms, `WITH … WHERE` scope, and compile-time checks of variables and
//! aggregates (checked on an empty graph, so no row is needed to find the error).

use graphbuff::{Database, Error};

fn db_with(setup: &str) -> Database {
    let db = Database::in_memory();
    for stmt in graphbuff::cypher::split_statements(setup).unwrap() {
        db.execute(stmt).unwrap_or_else(|e| panic!("setup failed on `{stmt}`: {e}"));
    }
    db
}

fn rows(db: &Database, q: &str) -> Vec<String> {
    let r = db.execute(q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    r.rows
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect()
}

fn semantic_error(db: &Database, q: &str) -> String {
    match db.execute(q) {
        Err(Error::Semantic(m)) => m,
        other => panic!("`{q}`: expected a semantic error, got {other:?}"),
    }
}

#[test]
fn numeric_literals() {
    let db = Database::in_memory();
    assert_eq!(rows(&db, "RETURN 0x1F, -0x1f, 0o17, -0o17"), ["31, -31, 15, -15"]);
    assert_eq!(rows(&db, "RETURN .5, -.5, .1e1"), ["0.5, -0.5, 1.0"]);
    assert_eq!(
        rows(&db, "RETURN -9223372036854775808, -0x8000000000000000, -0o1000000000000000000000"),
        ["-9223372036854775808, -9223372036854775808, -9223372036854775808"]
    );
    for bad in ["RETURN 9223372036854775808", "RETURN 0x8000000000000000", "RETURN 0x", "RETURN 0x1g", "RETURN 1e999"] {
        assert!(matches!(db.execute(bad), Err(Error::Syntax { .. })), "{bad}");
    }
    assert_eq!(rows(&db, "UNWIND [1, 2, 3] AS x RETURN [x, 0][1..2] AS l"), ["[0]", "[0]", "[0]"]);
}

#[test]
fn with_where_sees_incoming_variables() {
    let db = db_with("CREATE (:A {n: 1})-[:R]->(:B {n: 2}), (:A {n: 3})");
    assert_eq!(
        rows(&db, "MATCH (a:A) OPTIONAL MATCH (a)-[r]->() WITH a.n AS n WHERE r IS NULL RETURN n"),
        ["3"]
    );
    // A projected alias shadows the incoming variable of the same name.
    assert_eq!(rows(&db, "MATCH (a:A) WITH a.n AS a WHERE a > 2 RETURN a"), ["3"]);
    // After aggregating, only the projected columns are in scope.
    let m = semantic_error(&db, "MATCH (a:A) WITH count(*) AS c WHERE a.n > 0 RETURN c");
    assert!(m.contains("not defined"), "{m}");
}

#[test]
fn undefined_variables_fail_at_compile_time() {
    let db = Database::in_memory();
    for q in [
        "MATCH (n) RETURN m",
        "MATCH (n) WHERE n.x = missing RETURN n",
        "MATCH (n) WHERE (n)-[r]->(a) RETURN n",
        "MATCH (a) CREATE (a)-[:R]->(b {name: missing})",
        "MATCH (a) DELETE x",
        "MATCH (a) SET a.name = missing",
        "MERGE (n) ON MATCH SET x.num = 1",
        "MATCH (a) RETURN DISTINCT a.name ORDER BY a.age",
        "MATCH (a)--(b) RETURN count(b) AS c ORDER BY a.age + count(b)",
    ] {
        let m = semantic_error(&db, q);
        assert!(m.contains("not defined"), "`{q}`: {m}");
    }
    // Variables bound inside the expression are fine.
    let db = db_with("CREATE ({n: 1})");
    assert_eq!(
        rows(&db, "MATCH (a) RETURN [x IN [1, 2] WHERE x > a.n | x] AS l, reduce(s = 0, y IN [1, 2] | s + y) AS r, any(z IN [1] WHERE z = 1) AS q"),
        ["[2], 3, true"]
    );
}

#[test]
fn aggregates_in_order_by() {
    let db = db_with("CREATE (:P {age: 3})-[:K]->(:P {age: 4})");
    assert_eq!(
        rows(&db, "MATCH (a:P)-->(b:P) RETURN a.age AS age, count(b.age) AS c ORDER BY age, age + count(b.age)"),
        ["3, 1"]
    );
    for q in [
        "MATCH (n) WITH n.x AS foo ORDER BY count(n) RETURN foo",
        "MATCH (n) RETURN n.x ORDER BY max(n.y)",
        "MATCH (n) RETURN [x IN [1, 2] | count(*)]",
    ] {
        semantic_error(&Database::in_memory(), q);
    }
}
