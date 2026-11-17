//! Map projection, pattern comprehension and label expressions.

use graphbuff::{Database, Error, QueryResult};

fn db_with(setup: &str) -> Database {
    let db = Database::in_memory();
    for stmt in graphbuff::cypher::split_statements(setup).unwrap() {
        db.execute(stmt).unwrap_or_else(|e| panic!("setup failed on `{stmt}`: {e}"));
    }
    db
}

/// Runs `q` (and `EXPLAIN q`) and renders every row as a comma-separated string.
fn rows(db: &Database, q: &str) -> Vec<String> {
    let r: QueryResult = db.execute(q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    db.execute(&format!("EXPLAIN {q}")).unwrap_or_else(|e| panic!("EXPLAIN `{q}` failed: {e}"));
    r.rows
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect()
}

const GRAPH: &str = "
    CREATE (ann:Person {name: 'Ann', age: 34}),
           (bob:Person:Admin {name: 'Bob', age: 25}),
           (cat:Robot {name: 'Cat'}),
           (dot {name: 'Dot'}),
           (acme:Company {name: 'Acme'}),
           (ann)-[:KNOWS {since: 2010}]->(bob),
           (ann)-[:LIKES]->(cat),
           (bob)-[:KNOWS {since: 2015}]->(cat),
           (ann)-[:WORKS_AT]->(acme),
           (bob)-[:WORKS_AT]->(acme)
";

// ---- map projection ------------------------------------------------------------------------

#[test]
fn map_projection() {
    let db = db_with(GRAPH);
    assert_eq!(
        rows(&db, "MATCH (n:Person {name: 'Ann'}) RETURN n {.name, .missing, x: n.age + 1}"),
        [r#"{missing: null, name: "Ann", x: 35}"#]
    );
    assert_eq!(rows(&db, "MATCH (n:Person {name: 'Ann'}) RETURN n {.*}"), [r#"{age: 34, name: "Ann"}"#]);
    // Later entries replace earlier ones; a variable goes in under its own name.
    assert_eq!(
        rows(&db, "MATCH (n:Person {name: 'Ann'}) WITH n, 7 AS k RETURN n {.*, age: 0, k}"),
        [r#"{age: 0, k: 7, name: "Ann"}"#]
    );
    assert_eq!(
        rows(&db, "MATCH (:Person {name: 'Ann'})-[r:KNOWS]->() RETURN r {.since}"),
        ["{since: 2010}"]
    );
    assert_eq!(rows(&db, "WITH {a: 1, b: 2} AS m RETURN m {.a, c: 3}"), ["{a: 1, c: 3}"]);
    assert_eq!(rows(&db, "WITH {a: 1} AS m RETURN m {}"), ["{}"]);
    // null in, null out.
    assert_eq!(
        rows(&db, "OPTIONAL MATCH (n:Nobody) RETURN n {.name} AS m"),
        ["null"]
    );
    assert!(matches!(db.execute("WITH 1 AS x RETURN x {.a}"), Err(Error::Type(_))));
}

#[test]
fn map_projection_with_aggregation() {
    let db = db_with(GRAPH);
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person)-[:KNOWS]->(f) WITH p, f ORDER BY f.name \
             RETURN p {.name, friends: collect(f.name)} AS m ORDER BY m.name"
        ),
        [r#"{friends: ["Bob"], name: "Ann"}"#, r#"{friends: ["Cat"], name: "Bob"}"#]
    );
    // Elsewhere, a variable next to an aggregate must be a grouping key (as in Neo4j).
    assert_eq!(
        rows(&db, "MATCH (p:Person)-->(x) RETURN p, p.age + count(x) AS v ORDER BY v")
            .iter()
            .map(|r| r.rsplit(", ").next().unwrap().to_string())
            .collect::<Vec<_>>(),
        ["27", "37"]
    );
    for q in [
        "MATCH (p:Person)-->(x) RETURN p.age + count(x) AS v",
        "MATCH (p:Person)-->() WITH 10 AS k, p RETURN k + count(*) AS v",
        "MATCH (p:Person)-->(x) RETURN p.age + x.age AS g, p.age + x.age + count(*) AS v",
        "MATCH (p:Person)-->(x) RETURN p.name AS n, count(*) AS c ORDER BY p.age + count(*)",
        "MATCH (p:Person)-->(x) RETURN p {.name, other: x.name, n: count(*)}",
    ] {
        assert!(matches!(db.execute(q), Err(Error::Semantic(_))), "{q}");
    }
    assert_eq!(
        rows(&db, "MATCH (p:Person)-->(x) RETURN p.age AS a, count(*) AS c ORDER BY a + count(*), p.age + count(*)"),
        ["25, 2", "34, 3"]
    );
    // No rows, no groups.
    assert!(rows(&db, "MATCH (p:Nobody) RETURN p {.name, n: count(*)}").is_empty());
    assert_eq!(rows(&db, "MATCH (p:Nobody) RETURN count(*)"), ["0"]);
}

// ---- pattern comprehension -----------------------------------------------------------------

#[test]
fn pattern_comprehension() {
    let db = db_with(GRAPH);
    assert_eq!(
        rows(&db, "MATCH (a:Person {name: 'Ann'}) RETURN [(a)-[:KNOWS|LIKES]->(b) | b.name] AS names"),
        [r#"["Bob", "Cat"]"#]
    );
    assert_eq!(
        rows(&db, "MATCH (a:Person {name: 'Ann'}) RETURN [(a)-[r:KNOWS]->(b) WHERE r.since > 2000 | b.age * 2]"),
        ["[50]"]
    );
    // A named path, a list of maps, and no match at all.
    assert_eq!(
        rows(&db, "MATCH (a:Person {name: 'Bob'}) RETURN [p = (a)-->() | length(p)], [(a)<-[:LIKES]-(x) | x]"),
        ["[1, 1], []"]
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) RETURN p.name, size([(p)-[:WORKS_AT]->(c:Company) | c]) AS jobs ORDER BY p.name"
        ),
        ["\"Ann\", 1", "\"Bob\", 1"]
    );
    // Variables bound inside do not leak out.
    assert!(db.execute("MATCH (a:Person) WITH [(a)-->(b) | b] AS l RETURN b").is_err());
    // Nested in a map projection, and inside a list comprehension.
    assert_eq!(
        rows(
            &db,
            "MATCH (c:Company) RETURN c {.name, staff: [(c)<-[:WORKS_AT]-(p) | p.name]} AS m"
        )
        .len(),
        1
    );
    assert_eq!(
        rows(&db, "MATCH (a:Person {name: 'Ann'}) RETURN [x IN [a] | size([(x)-->(y) | y])]"),
        ["[3]"]
    );
    // A list starting with a parenthesized expression is still a list.
    assert_eq!(rows(&db, "RETURN [(1 + 2), 4]"), ["[3, 4]"]);
}

// ---- label expressions ---------------------------------------------------------------------

#[test]
fn label_expressions_on_nodes() {
    let db = db_with(GRAPH);
    let names = |q: &str| rows(&db, &format!("MATCH {q} RETURN n.name ORDER BY n.name"));
    assert_eq!(names("(n:Person|Robot)"), ["\"Ann\"", "\"Bob\"", "\"Cat\""]);
    assert_eq!(names("(n:Person&Admin)"), ["\"Bob\""]);
    assert_eq!(names("(n:Person&!Admin)"), ["\"Ann\""]);
    assert_eq!(names("(n:!Person)"), ["\"Acme\"", "\"Cat\"", "\"Dot\""]);
    assert_eq!(names("(n:%)").len(), 4);
    assert_eq!(names("(n:!%)"), ["\"Dot\""]);
    assert_eq!(names("(n:(Robot|Company)&!Person)"), ["\"Acme\"", "\"Cat\""]);
    assert_eq!(names("(:Person {name: 'Ann'})-->(n:Robot|Company)"), ["\"Acme\"", "\"Cat\""]);
    // In WHERE, and the old `:A:B` form.
    assert_eq!(
        rows(&db, "MATCH (n) WHERE n:Robot|Company RETURN n.name ORDER BY n.name"),
        ["\"Acme\"", "\"Cat\""]
    );
    assert_eq!(rows(&db, "MATCH (n) WHERE n:Person:Admin RETURN n.name"), ["\"Bob\""]);
    assert_eq!(rows(&db, "MATCH (n) RETURN count(CASE WHEN n:!% THEN 1 END)"), ["1"]);
    // In a list comprehension's WHERE, `|` starts the mapping; parenthesize to use it.
    assert_eq!(
        rows(&db, "MATCH (n) WITH collect(n) AS ns RETURN [x IN ns WHERE x:Robot | x.name]"),
        [r#"["Cat"]"#]
    );
    assert_eq!(
        rows(&db, "MATCH (n) WITH collect(n) AS ns RETURN size([x IN ns WHERE (x:Robot|Company)])"),
        ["2"]
    );
    assert_eq!(rows(&db, "MATCH (n:Person) RETURN count(*)"), ["2"]);
    // CREATE and MERGE need plain labels.
    for q in ["CREATE (:A|B)", "MERGE (:!A)", "CREATE ()-[:!R]->()"] {
        assert!(matches!(db.execute(q), Err(Error::Semantic(_))), "{q}");
    }
}

#[test]
fn label_expressions_on_relationships() {
    let db = db_with(GRAPH);
    let types = |q: &str| rows(&db, &format!("MATCH (:Person {{name: 'Ann'}})-[r{q}]->() RETURN type(r) ORDER BY type(r)"));
    assert_eq!(types(":KNOWS|LIKES"), ["\"KNOWS\"", "\"LIKES\""]);
    assert_eq!(types(":!KNOWS"), ["\"LIKES\"", "\"WORKS_AT\""]);
    assert_eq!(types(":!(KNOWS|LIKES)"), ["\"WORKS_AT\""]);
    assert_eq!(types(":%"), ["\"KNOWS\"", "\"LIKES\"", "\"WORKS_AT\""]);
    assert!(types(":KNOWS&LIKES").is_empty());
    // Over variable-length and shortest paths too.
    assert_eq!(
        rows(&db, "MATCH (:Person {name: 'Ann'})-[:!WORKS_AT*1..3]->(n) RETURN DISTINCT n.name ORDER BY n.name"),
        ["\"Bob\"", "\"Cat\""]
    );
    assert_eq!(
        rows(
            &db,
            "MATCH p = shortestPath((:Person {name: 'Ann'})-[:!LIKES*]->(:Robot)) RETURN length(p)"
        ),
        ["2"]
    );
    // In WHERE, a relationship's type is its label.
    assert_eq!(
        rows(&db, "MATCH ()-[r]->() WHERE r:KNOWS RETURN count(r)"),
        ["2"]
    );
    assert_eq!(
        rows(&db, "MATCH ()-[r]->() WHERE r:!KNOWS&!WORKS_AT RETURN count(r)"),
        ["1"]
    );
}
