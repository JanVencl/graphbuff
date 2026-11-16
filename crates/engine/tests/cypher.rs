//! End-to-end tests: Cypher text in, results out.

use graphbuff::{Database, Error, QueryResult, ResultValue, params};

fn db_with(setup: &str) -> Database {
    let db = Database::in_memory();
    for stmt in graphbuff::cypher::split_statements(setup).unwrap() {
        db.execute(stmt)
            .unwrap_or_else(|e| panic!("setup failed on `{stmt}`: {e}"));
    }
    db
}

/// Runs `q`, and checks that `EXPLAIN` can show its plan too.
fn run(db: &Database, q: &str) -> QueryResult {
    let r = db
        .execute(q)
        .unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    let plain = q.trim_start();
    if !["EXPLAIN", "PROFILE"].iter().any(|k| plain.len() >= 7 && plain[..7].eq_ignore_ascii_case(k)) {
        let plan = db
            .execute(&format!("EXPLAIN {q}"))
            .unwrap_or_else(|e| panic!("EXPLAIN `{q}` failed: {e}"));
        assert!(!plan.plan.expect("a plan").lines.is_empty());
    }
    r
}

/// Renders every row as a comma-separated string, for compact assertions.
fn rows(r: &QueryResult) -> Vec<String> {
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect()
}

fn scalar(db: &Database, q: &str) -> ResultValue {
    let r = run(db, q);
    assert_eq!(
        r.rows.len(),
        1,
        "expected one row from `{q}`, got {:?}",
        rows(&r)
    );
    r.rows[0][0].clone()
}

const SOCIAL: &str = "
    CREATE (ann:Person {name: 'Ann', age: 34}),
           (bob:Person {name: 'Bob', age: 25}),
           (cat:Person {name: 'Cat', age: 41}),
           (dan:Person:Admin {name: 'Dan', age: 29}),
           (acme:Company {name: 'Acme'}),
           (ann)-[:KNOWS {since: 2010}]->(bob),
           (bob)-[:KNOWS {since: 2015}]->(cat),
           (cat)-[:KNOWS {since: 2020}]->(dan),
           (ann)-[:WORKS_AT]->(acme),
           (dan)-[:WORKS_AT]->(acme);
";

#[test]
fn literals_and_expressions() {
    let db = Database::in_memory();
    assert_eq!(
        rows(&run(
            &db,
            "RETURN 1 + 2 * 3 AS a, 7 / 2 AS b, 7.0 / 2 AS c, 2 ^ 3 AS d, -5 % 3 AS e"
        )),
        vec!["7, 3, 3.5, 8.0, -2"]
    );
    assert_eq!(
        rows(&run(
            &db,
            "RETURN 'a' + 'b' AS s, [1, 2] + 3 AS l, {x: 1}.x AS m, [1,2,3][-1] AS i, [1,2,3,4][1..3] AS sl"
        )),
        vec!["\"ab\", [1, 2, 3], 1, 3, [2, 3]"]
    );
    assert_eq!(scalar(&db, "RETURN null = null"), ResultValue::Null);
    assert_eq!(scalar(&db, "RETURN null OR true"), ResultValue::Bool(true));
    assert_eq!(
        scalar(&db, "RETURN null AND false"),
        ResultValue::Bool(false)
    );
    assert_eq!(scalar(&db, "RETURN 1 < 2 < 3"), ResultValue::Bool(true));
    assert_eq!(scalar(&db, "RETURN 3 IN [1, 2, null]"), ResultValue::Null);
    assert_eq!(
        scalar(&db, "RETURN 'graphbuff' STARTS WITH 'graph'"),
        ResultValue::Bool(true)
    );
    assert_eq!(
        scalar(
            &db,
            "RETURN CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END"
        )
        .as_str(),
        Some("two")
    );
    assert_eq!(
        scalar(&db, "RETURN CASE WHEN 1 > 2 THEN 'x' END"),
        ResultValue::Null
    );
}

#[test]
fn functions() {
    let db = Database::in_memory();
    assert_eq!(
        rows(&run(
            &db,
            "RETURN toUpper('abc'), size('héllo'), substring('graphbuff', 5), split('a,b', ','), \
             range(1, 5, 2), coalesce(null, 3), toInteger('42'), toFloat('1.5'), abs(-3), reverse([1, 2])"
        )),
        vec!["\"ABC\", 5, \"buff\", [\"a\", \"b\"], [1, 3, 5], 3, 42, 1.5, 3, [2, 1]"]
    );
    assert_eq!(
        rows(&run(
            &db,
            "RETURN [x IN range(1, 6) WHERE x % 2 = 0 | x * 10] AS l, all(x IN [1, 2] WHERE x > 0) AS a"
        )),
        vec!["[20, 40, 60], true"]
    );
    assert!(matches!(
        db.execute("RETURN nope(1)"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn create_and_match() {
    let db = db_with(SOCIAL);
    assert_eq!(db.node_count(), 5);
    assert_eq!(db.relationship_count(), 5);

    let r = run(
        &db,
        "MATCH (p:Person) WHERE p.age > 28 RETURN p.name AS name ORDER BY name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\"", "\"Cat\"", "\"Dan\""]);

    let r = run(
        &db,
        "MATCH (a:Person {name: 'Ann'})-[k:KNOWS]->(b) RETURN b.name, k.since",
    );
    assert_eq!(rows(&r), vec!["\"Bob\", 2010"]);

    // Incoming and undirected.
    let r = run(
        &db,
        "MATCH (b:Person {name: 'Bob'})<-[:KNOWS]-(a) RETURN a.name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\""]);
    let r = run(
        &db,
        "MATCH (b:Person {name: 'Bob'})-[:KNOWS]-(x) RETURN x.name ORDER BY x.name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\"", "\"Cat\""]);

    // Multiple labels and label predicates.
    assert_eq!(
        rows(&run(&db, "MATCH (n:Person:Admin) RETURN n.name")),
        vec!["\"Dan\""]
    );
    assert_eq!(
        rows(&run(&db, "MATCH (n) WHERE n:Company RETURN n.name")),
        vec!["\"Acme\""]
    );

    // Multi-hop chain and comma-separated patterns joined on a shared variable.
    let r = run(
        &db,
        "MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN a.name, c.name ORDER BY a.name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\", \"Cat\"", "\"Bob\", \"Dan\""]);
    let r = run(
        &db,
        "MATCH (p:Person)-[:WORKS_AT]->(c), (q:Person)-[:WORKS_AT]->(c) WHERE p.name < q.name RETURN p.name, q.name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\", \"Dan\""]);
}

#[test]
fn match_returns_entities() {
    let db = db_with(SOCIAL);
    let r = run(
        &db,
        "MATCH (a:Person {name: 'Ann'})-[k:KNOWS]->(b) RETURN a, k",
    );
    let a = r.get(0, "a").and_then(ResultValue::as_node).unwrap();
    assert_eq!(a.labels, vec!["Person"]);
    assert_eq!(a.properties["name"], "Ann".into());
    let k = r
        .get(0, "k")
        .and_then(ResultValue::as_relationship)
        .unwrap();
    assert_eq!(k.rel_type, "KNOWS");
    assert_eq!(k.start, a.id);
    assert_eq!(
        scalar(&db, "MATCH ()-[k:KNOWS {since: 2010}]->() RETURN type(k)").as_str(),
        Some("KNOWS")
    );
}

#[test]
fn relationship_uniqueness_and_cycles() {
    let db = db_with("CREATE (a:N {id: 1})-[:R]->(b:N {id: 2})-[:R]->(a)");
    // Each relationship is used once per match: no a->b->a->b... loops.
    let r = run(
        &db,
        "MATCH (x)-[:R]->(y)-[:R]->(z) RETURN x.id, z.id ORDER BY x.id",
    );
    assert_eq!(rows(&r), vec!["1, 1", "2, 2"]);
    // Repeated variable forces a cycle.
    let r = run(&db, "MATCH (x)-[:R]->(y)-[:R]->(x) RETURN count(*)");
    assert_eq!(rows(&r), vec!["2"]);
}

#[test]
fn variable_length_paths() {
    let db = db_with(
        "CREATE (:S {i: 0})-[:NEXT]->(:S {i: 1})-[:NEXT]->(:S {i: 2})-[:NEXT]->(:S {i: 3})",
    );
    let r = run(
        &db,
        "MATCH (a:S {i: 0})-[:NEXT*]->(b) RETURN b.i ORDER BY b.i",
    );
    assert_eq!(rows(&r), vec!["1", "2", "3"]);
    let r = run(
        &db,
        "MATCH (a:S {i: 0})-[:NEXT*2..]->(b) RETURN b.i ORDER BY b.i",
    );
    assert_eq!(rows(&r), vec!["2", "3"]);
    let r = run(
        &db,
        "MATCH (a:S {i: 0})-[:NEXT*0..1]->(b) RETURN b.i ORDER BY b.i",
    );
    assert_eq!(rows(&r), vec!["0", "1"]);
    let r = run(
        &db,
        "MATCH (a:S {i: 0})-[rs:NEXT*3]->(b) RETURN size(rs), b.i",
    );
    assert_eq!(rows(&r), vec!["3, 3"]);
    // Reverse direction from the other end.
    let r = run(
        &db,
        "MATCH (a)-[:NEXT*1..2]->(b:S {i: 3}) RETURN a.i ORDER BY a.i",
    );
    assert_eq!(rows(&r), vec!["1", "2"]);
}

#[test]
fn paths() {
    let db = db_with("CREATE (:A {n: 'a'})-[:R]->(:B {n: 'b'})-[:R]->(:C {n: 'c'})");
    let r = run(
        &db,
        "MATCH p = (a:A)-[:R*]->(c:C) RETURN length(p), [n IN nodes(p) | n.n] AS names",
    );
    assert_eq!(rows(&r), vec!["2, [\"a\", \"b\", \"c\"]"]);
    // Path assembled in pattern order even when matched from the right-hand side.
    let r = run(
        &db,
        "MATCH (c:C) MATCH p = (x)-[:R]->(y)-[:R]->(c) RETURN [n IN nodes(p) | n.n]",
    );
    assert_eq!(rows(&r), vec!["[\"a\", \"b\", \"c\"]"]);
}

#[test]
fn optional_match() {
    let db = db_with(SOCIAL);
    let r = run(
        &db,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:WORKS_AT]->(c) RETURN p.name, c.name ORDER BY p.name",
    );
    assert_eq!(
        rows(&r),
        vec![
            "\"Ann\", \"Acme\"",
            "\"Bob\", null",
            "\"Cat\", null",
            "\"Dan\", \"Acme\""
        ]
    );
}

#[test]
fn aggregation() {
    let db = db_with(SOCIAL);
    assert_eq!(
        scalar(&db, "MATCH (p:Person) RETURN count(*)"),
        ResultValue::Int(4)
    );
    assert_eq!(
        scalar(&db, "MATCH (p:Person) RETURN avg(p.age)"),
        ResultValue::Float(32.25)
    );
    assert_eq!(
        rows(&run(
            &db,
            "MATCH (p:Person) RETURN min(p.age), max(p.age), sum(p.age), collect(p.name)[0]"
        )),
        vec!["25, 41, 129, \"Ann\""]
    );
    // Implicit grouping.
    let r = run(
        &db,
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:WORKS_AT]->(c) \
         RETURN c.name AS company, count(p) AS n ORDER BY n DESC",
    );
    // Groups keep first-seen order and the sort is stable.
    assert_eq!(rows(&r), vec!["\"Acme\", 2", "null, 2"]);
    // Aggregates over empty input still return one row.
    assert_eq!(
        scalar(&db, "MATCH (x:Nope) RETURN count(x)"),
        ResultValue::Int(0)
    );
    assert_eq!(
        scalar(&db, "MATCH (x:Nope) RETURN sum(x.v)"),
        ResultValue::Int(0)
    );
    // ...unless grouped.
    assert!(run(&db, "MATCH (x:Nope) RETURN x.k, count(*)").is_empty());
    // DISTINCT inside aggregates and expressions around aggregates.
    assert_eq!(
        scalar(&db, "UNWIND [1, 1, 2, null] AS x RETURN count(DISTINCT x)"),
        ResultValue::Int(2)
    );
    assert_eq!(
        scalar(&db, "UNWIND [1, 2, 3] AS x RETURN sum(x) * 10 + count(*)"),
        ResultValue::Int(63)
    );
    // ORDER BY an aggregate that's also projected.
    let r = run(
        &db,
        "MATCH (p)-[r]->() RETURN type(r) AS t, count(*) ORDER BY count(*) DESC, t",
    );
    assert_eq!(rows(&r), vec!["\"KNOWS\", 3", "\"WORKS_AT\", 2"]);
}

#[test]
fn with_pipeline() {
    let db = db_with(SOCIAL);
    let r = run(
        &db,
        "MATCH (p:Person)-[:KNOWS]->(f) WITH p, count(f) AS friends WHERE friends >= 1 \
         RETURN p.name ORDER BY p.name LIMIT 2",
    );
    assert_eq!(rows(&r), vec!["\"Ann\"", "\"Bob\""]);
    let r = run(
        &db,
        "MATCH (p:Person) WITH p ORDER BY p.age DESC LIMIT 1 RETURN p.name",
    );
    assert_eq!(rows(&r), vec!["\"Cat\""]);
    let r = run(
        &db,
        "MATCH (p:Person) RETURN p.name ORDER BY p.age SKIP 1 LIMIT 2",
    );
    assert_eq!(rows(&r), vec!["\"Dan\"", "\"Ann\""]);
    let r = run(
        &db,
        "MATCH (p:Person) RETURN DISTINCT size(labels(p)) AS n ORDER BY n",
    );
    assert_eq!(rows(&r), vec!["1", "2"]);
    // Variables not carried through WITH go out of scope.
    assert!(matches!(
        db.execute("MATCH (p:Person) WITH p.name AS n RETURN p"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn unwind_and_params() {
    let db = Database::in_memory();
    let p = params! { "names" => vec!["x", "y", "z"], "min" => 1 };
    let r = db
        .execute_with_params(
            "UNWIND $names AS n CREATE (:Item {name: n}) RETURN count(*) AS created",
            &p,
        )
        .unwrap();
    assert_eq!(rows(&r), vec!["3"]);
    assert_eq!(r.stats.nodes_created, 3);
    let r = db
        .execute_with_params(
            "MATCH (i:Item) WITH i ORDER BY i.name SKIP $min RETURN collect(i.name)",
            &p,
        )
        .unwrap();
    assert_eq!(rows(&r), vec!["[\"y\", \"z\"]"]);
    assert!(matches!(
        db.execute("RETURN $missing"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn set_and_remove() {
    let db = db_with(SOCIAL);
    let r = run(
        &db,
        "MATCH (p:Person {name: 'Bob'}) SET p.age = p.age + 1, p:Senior, p += {city: 'Oslo'} RETURN p.age",
    );
    assert_eq!(rows(&r), vec!["26"]);
    assert_eq!(r.stats.properties_set, 2);
    assert_eq!(r.stats.labels_added, 1);
    assert_eq!(
        rows(&run(&db, "MATCH (p:Senior) RETURN p.city")),
        vec!["\"Oslo\""]
    );

    run(
        &db,
        "MATCH (p:Person {name: 'Bob'}) SET p = {name: 'Robert'}",
    );
    assert_eq!(
        rows(&run(&db, "MATCH (p:Senior) RETURN keys(p)")),
        vec!["[\"name\"]"]
    );

    let r = run(&db, "MATCH (p:Senior) REMOVE p:Senior, p.name");
    assert_eq!(r.stats.labels_removed, 1);
    assert_eq!(r.stats.properties_set, 1);
    assert!(run(&db, "MATCH (p:Senior) RETURN p").is_empty());

    // Setting null removes; relationships have properties too.
    run(
        &db,
        "MATCH ()-[k:KNOWS {since: 2010}]->() SET k.since = null, k.weight = 0.5",
    );
    assert_eq!(
        rows(&run(
            &db,
            "MATCH ()-[k:KNOWS {weight: 0.5}]->() RETURN k.since"
        )),
        vec!["null"]
    );
}

#[test]
fn delete() {
    let db = db_with(SOCIAL);
    let err = db
        .execute("MATCH (p:Person {name: 'Ann'}) DELETE p")
        .unwrap_err();
    assert!(matches!(err, Error::Constraint(_)), "{err}");
    assert_eq!(db.node_count(), 5, "failed query must not change anything");

    let r = run(&db, "MATCH (p:Person {name: 'Ann'}) DETACH DELETE p");
    assert_eq!(
        (r.stats.nodes_deleted, r.stats.relationships_deleted),
        (1, 2)
    );
    assert_eq!((db.node_count(), db.relationship_count()), (4, 3));

    // Deleting a node together with its relationships in one DELETE works in any order.
    let r = run(&db, "MATCH (c:Company)<-[w]-() DELETE c, w");
    assert_eq!(
        (r.stats.nodes_deleted, r.stats.relationships_deleted),
        (1, 1)
    );

    run(&db, "MATCH (n) DETACH DELETE n");
    assert_eq!((db.node_count(), db.relationship_count()), (0, 0));
}

#[test]
fn merge() {
    let db = Database::in_memory();
    for _ in 0..3 {
        run(
            &db,
            "MERGE (p:Person {name: 'Ann'}) ON CREATE SET p.visits = 1 ON MATCH SET p.visits = p.visits + 1",
        );
    }
    assert_eq!(db.node_count(), 1);
    assert_eq!(
        scalar(&db, "MATCH (p:Person) RETURN p.visits"),
        ResultValue::Int(3)
    );

    // MERGE a relationship between existing nodes.
    run(&db, "CREATE (:Person {name: 'Bob'})");
    for _ in 0..2 {
        run(
            &db,
            "MATCH (a:Person {name: 'Ann'}), (b:Person {name: 'Bob'}) MERGE (a)-[:KNOWS]->(b)",
        );
    }
    assert_eq!(db.relationship_count(), 1);

    // MERGE sees nodes created by earlier rows of the same query.
    run(&db, "UNWIND ['x', 'y', 'x'] AS n MERGE (:Tag {name: n})");
    assert_eq!(
        scalar(&db, "MATCH (t:Tag) RETURN count(t)"),
        ResultValue::Int(2)
    );
}

#[test]
fn pattern_predicates() {
    let db = db_with(SOCIAL);
    let r = run(
        &db,
        "MATCH (p:Person) WHERE (p)-[:WORKS_AT]->(:Company) RETURN p.name ORDER BY p.name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\"", "\"Dan\""]);
    let r = run(
        &db,
        "MATCH (p:Person) WHERE NOT exists((p)-[:KNOWS]->()) RETURN p.name",
    );
    assert_eq!(rows(&r), vec!["\"Dan\""]);
    let r = run(
        &db,
        "MATCH (p:Person) WHERE exists(p.age) AND p.age < 26 RETURN p.name",
    );
    assert_eq!(rows(&r), vec!["\"Bob\""]);
}

#[test]
fn union() {
    let db = db_with(SOCIAL);
    let r = run(
        &db,
        "MATCH (p:Person) RETURN p.name AS n UNION MATCH (c:Company) RETURN c.name AS n",
    );
    assert_eq!(r.len(), 5);
    let r = run(&db, "RETURN 1 AS x UNION RETURN 1 AS x");
    assert_eq!(r.len(), 1);
    let r = run(&db, "RETURN 1 AS x UNION ALL RETURN 1 AS x");
    assert_eq!(r.len(), 2);
    assert!(db.execute("RETURN 1 AS x UNION RETURN 1 AS y").is_err());
}

#[test]
fn failed_writes_roll_back() {
    let db = Database::in_memory();
    let err = db
        .execute("CREATE (a:A {x: 1}) WITH a CREATE (b:B {x: 1 / 0})")
        .unwrap_err();
    assert!(matches!(err, Error::Runtime(_)));
    assert_eq!(db.node_count(), 0);

    run(&db, "CREATE (:A {v: 1})");
    assert!(
        db.execute("MATCH (a:A) SET a.v = 2, a:Changed WITH a SET a.bad = {nested: 1}")
            .is_err()
    );
    assert_eq!(scalar(&db, "MATCH (a:A) RETURN a.v"), ResultValue::Int(1));
    assert!(run(&db, "MATCH (a:Changed) RETURN a").is_empty());
}

#[test]
fn semantic_errors() {
    let db = Database::in_memory();
    assert!(matches!(db.execute("MATCH (n)"), Err(Error::Semantic(_))));
    assert!(matches!(
        db.execute("RETURN 1 RETURN 2"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(db.execute("RETURN x"), Err(Error::Semantic(_))));
    assert!(matches!(
        db.execute("CREATE (a)-[:R]-(b)"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.execute("CREATE (a)-[]->(b)"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.execute("MATCH (n) WHERE count(n) > 1 RETURN n"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.execute("MATCH (n RETURN n"),
        Err(Error::Syntax { .. })
    ));
    assert!(matches!(
        db.execute("CALL db.nope()"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn compile_time_type_errors() {
    let db = db_with("CREATE (:A)-[:R]->(:B)");
    for bad in [
        "RETURN NOT 1",
        "RETURN NOT {}",
        "RETURN [] AND true",
        "RETURN false OR 'x'",
        "RETURN true XOR 1.5",
        "RETURN 'a' % 2",
        "RETURN 1 IN 'abc'",
        "RETURN none(x IN ['a', 'b'] WHERE x % 2 = 0)",
        "MATCH (n) RETURN length(n)",
        "MATCH ()-[r]->() RETURN length(r)",
        "MATCH p = ()-->() RETURN size(p)",
        "MATCH (n) WHERE n RETURN n",
        "MATCH (n) RETURN type(n)",
        "MATCH p = ()-->() RETURN labels(p)",
        "MATCH p = ()-->() WHERE p.name = 'x' RETURN p",
        "MATCH (n) DELETE 1 + 1",
    ] {
        assert!(matches!(db.execute(bad), Err(Error::Semantic(_))), "{bad}");
    }
    for bad in ["WITH 123 AS x RETURN x.num", "WITH [1, true] AS x RETURN x.num", "RETURN range(0.0, 1)"] {
        assert!(matches!(db.execute(bad), Err(Error::Type(_))), "{bad}");
    }
    // Types not known before the query runs are left to the runtime.
    for good in [
        "RETURN NOT null, null AND true, 1 IN null",
        "WITH $p AS x RETURN x",
        "MATCH (n) RETURN NOT n.flag",
        "RETURN date('2020-01-02') - duration('P1D')",
        "RETURN size('abc'), size([1]), length('abc')",
        "MATCH p = ()-->() RETURN length(p), size(nodes(p))",
        "MATCH (n) RETURN properties(n), labels(n)",
    ] {
        let mut params = std::collections::BTreeMap::new();
        params.insert("p".to_string(), graphbuff::Value::Int(1));
        db.execute_with_params(good, &params).unwrap_or_else(|e| panic!("`{good}` failed: {e}"));
    }
}

#[test]
fn pattern_variable_kinds() {
    let db = db_with("CREATE (:A)-[:R]->(:B)-[:R]->(:C)");
    for bad in [
        // A variable keeps its kind (VariableTypeConflict).
        "MATCH ()-[r]-() MATCH (r) RETURN r",
        "MATCH (r) MATCH ()-[r]-() RETURN r",
        "MATCH p = ()-[]-() MATCH (p) RETURN p",
        "MATCH ()-[r]-(r) RETURN r",
        "MATCH (n) WITH [n] AS users MATCH (users)-->(m) RETURN m",
        "WITH 1 AS n MATCH (n) RETURN n",
        // A path variable must be new; a relationship is used once (VariableAlreadyBound).
        "MATCH (p)-[]-() MATCH p = ()-[]-() RETURN p",
        "MATCH p = (p)-[]-() RETURN p",
        "MATCH ()-[r]->()-[r]->() RETURN r",
        "MATCH (a) CREATE (a:L)",
        "MATCH ()-[r]->() CREATE ()-[r:R]->()",
    ] {
        assert!(matches!(db.execute(bad), Err(Error::Semantic(_))), "{bad}");
    }
    for good in [
        "MATCH (a)-[r]->(b) MATCH (a)-[r]->(c) RETURN a, b, c",
        "MATCH ()-[r1]->()-[r2]->() WITH [r1, r2] AS rs LIMIT 1 MATCH (x)-[rs*]->(y) RETURN x, y",
        "MATCH (a:A) WITH a MATCH (a)-->(b) RETURN b",
        "MATCH (a:A) CREATE (a)-[:S]->(:D)",
        "WITH null AS n MATCH (n) RETURN n",
    ] {
        run(&db, good);
    }
}

#[test]
fn table_output() {
    let db = db_with("CREATE (:P {name: 'Ann'})");
    let table = run(&db, "MATCH (p:P) RETURN p.name AS name, 1 AS one").to_table();
    assert_eq!(
        table,
        "+-------+-----+\n| name  | one |\n+-------+-----+\n| \"Ann\" | 1   |\n+-------+-----+"
    );
}

#[test]
fn index_commands() {
    let db = db_with(SOCIAL);
    let r = run(&db, "CREATE INDEX person_name FOR (p:Person) ON (p.name)");
    assert_eq!(r.stats.indexes_added, 1);
    run(&db, "CREATE INDEX ON :Person(age)");
    // Duplicates fail unless IF NOT EXISTS.
    assert!(matches!(
        db.execute("CREATE INDEX other FOR (p:Person) ON (p.name)"),
        Err(Error::Constraint(_))
    ));
    let r = run(
        &db,
        "CREATE INDEX person_name IF NOT EXISTS FOR (p:Person) ON (p.name)",
    );
    assert_eq!(r.stats.indexes_added, 0);
    assert_eq!(
        rows(&run(&db, "SHOW INDEXES")),
        vec![
            "\"index_Person_age\", \"NODE\", \"Person\", [\"age\"]",
            "\"person_name\", \"NODE\", \"Person\", [\"name\"]"
        ]
    );
    run(&db, "DROP INDEX ON :Person(age)");
    run(&db, "DROP INDEX person_name");
    assert!(run(&db, "SHOW INDEXES").is_empty());
    assert!(matches!(
        db.execute("DROP INDEX nope"),
        Err(Error::Semantic(_))
    ));
    run(&db, "DROP INDEX nope IF EXISTS");
    assert!(matches!(
        db.execute("MATCH (n) CREATE INDEX FOR (p:Person) ON (p.name)"),
        Err(Error::Syntax { .. } | Error::Semantic(_))
    ));
}

/// Every query must return the same rows with and without indexes.
#[test]
fn indexes_never_change_results() {
    let setup = "
        UNWIND range(0, 199) AS i
        CREATE (:Item {id: i, bucket: i % 10, tag: CASE WHEN i % 3 = 0 THEN 'x' ELSE 'y' END, score: i * 1.0});
        MATCH (a:Item), (b:Item) WHERE b.id = a.id + 1 AND a.bucket = 0 CREATE (a)-[:NEXT]->(b);
    ";
    let plain = db_with(setup);
    let indexed = db_with(setup);
    for idx in [
        "CREATE INDEX FOR (n:Item) ON (n.id)",
        "CREATE INDEX FOR (n:Item) ON (n.bucket)",
        "CREATE INDEX FOR (n:Item) ON (n.tag)",
        "CREATE INDEX FOR (n:Item) ON (n.score)",
    ] {
        run(&indexed, idx);
    }
    let queries = [
        "MATCH (n:Item {id: 42}) RETURN n.id",
        "MATCH (n:Item) WHERE n.id = 42 RETURN n.id",
        "MATCH (n:Item) WHERE 42 = n.id AND n.tag = 'x' RETURN n.id",
        "MATCH (n:Item) WHERE n.id = 42.0 RETURN n.id",
        "MATCH (n:Item) WHERE n.score = 7 RETURN n.id",
        "MATCH (n:Item) WHERE n.id IN [1, 2, 3, 2, null, 'x'] RETURN n.id ORDER BY n.id",
        "MATCH (n:Item) WHERE n.id = 1 OR n.id = 2 RETURN n.id ORDER BY n.id",
        "MATCH (n:Item) WHERE n.id = null RETURN n.id",
        "MATCH (n:Item {bucket: 3}) WHERE n.id > 100 RETURN count(n)",
        "MATCH (a:Item {bucket: 0}), (b:Item) WHERE b.id = a.id + 1 RETURN a.id, b.id ORDER BY a.id",
        "MATCH (b:Item), (a:Item {id: 10}) WHERE b.id = a.id + 5 RETURN b.id",
        "MATCH (a:Item)-[:NEXT]->(b:Item {id: 31}) RETURN a.id",
        "MATCH (a:Item {id: 30})-[:NEXT]->(b) RETURN b.id",
        "UNWIND [5, 6, 500] AS k MATCH (n:Item {id: k}) RETURN k, n.bucket ORDER BY k",
        "UNWIND [5, 6] AS k MATCH (n:Item) WHERE n.id = k * 2 RETURN n.id ORDER BY n.id",
        "MATCH (n:Item) WHERE n.id = n.bucket RETURN n.id ORDER BY n.id",
        "MATCH (n:Item) WHERE n.tag = 'x' AND n.bucket = 3 RETURN count(*)",
        "MATCH (n:Item) WHERE [x IN [n.id] WHERE x = 3] <> [] AND n.id = 3 RETURN n.id",
        "MATCH (n:Item) OPTIONAL MATCH (m:Item) WHERE m.id = n.id + 1000 RETURN count(n), count(m)",
    ];
    for q in queries {
        assert_eq!(
            rows(&run(&plain, q)),
            rows(&run(&indexed, q)),
            "results differ for `{q}`"
        );
    }

    // Writes keep the index consistent.
    let params = params! { "ids" => vec![1, 2, 3] };
    for db in [&plain, &indexed] {
        db.execute_with_params(
            "MATCH (n:Item) WHERE n.id IN $ids SET n.id = n.id + 1000",
            &params,
        )
        .unwrap();
        run(db, "MATCH (n:Item {id: 50}) REMOVE n:Item");
        run(db, "MATCH (n {id: 50}) SET n:Item, n.id = 5000");
        run(db, "MATCH (n:Item {id: 60}) DETACH DELETE n");
        assert!(
            db.execute("MATCH (n:Item {id: 70}) SET n.id = 7000 WITH n RETURN 1 / 0")
                .is_err()
        );
    }
    for q in [
        "MATCH (n:Item) WHERE n.id IN [1, 2, 3, 1001, 1002, 1003] RETURN n.id ORDER BY n.id",
        "MATCH (n:Item {id: 50}) RETURN n.id",
        "MATCH (n:Item {id: 5000}) RETURN n.bucket",
        "MATCH (n:Item {id: 60}) RETURN n",
        "MATCH (n:Item {id: 70}) RETURN n.id",
        "MATCH (n:Item {id: 7000}) RETURN n.id",
    ] {
        assert_eq!(
            rows(&run(&plain, q)),
            rows(&run(&indexed, q)),
            "results differ for `{q}`"
        );
    }
    assert_eq!(
        rows(&run(&indexed, "MATCH (n:Item {id: 70}) RETURN n.id")),
        vec!["70"]
    );
}

// ---- LIMIT pushdown ---------------------------------------------------------------------

/// A LIMIT must return exactly the leading rows of the unlimited query.
fn assert_prefix(db: &Database, q: &str, skip: usize, limit: usize) {
    let all = rows(&run(db, q));
    let limited = rows(&run(db, &format!("{q} SKIP {skip} LIMIT {limit}")));
    let expected: Vec<String> = all.into_iter().skip(skip).take(limit).collect();
    assert_eq!(limited, expected, "`{q}` SKIP {skip} LIMIT {limit}");
}

#[test]
fn limit_returns_the_leading_rows() {
    let db = db_with(SOCIAL);
    let queries = [
        "MATCH (p:Person) RETURN p.name",
        "MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name",
        "MATCH (a)-[*1..3]-(b) RETURN a.name, b.name",
        "MATCH (a:Person), (b:Person) RETURN a.name, b.name",
        "MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(f) RETURN p.name, f.name",
        "MATCH (p:Person) WHERE p.age > 26 RETURN p.name",
        "UNWIND range(1, 20) AS x UNWIND range(1, x) AS y RETURN x, y",
        "MATCH (p:Person) WITH p UNWIND [1, 2] AS i RETURN p.name, i",
    ];
    for q in queries {
        for (skip, limit) in [(0, 0), (0, 1), (0, 3), (2, 2), (1, 100), (100, 1)] {
            assert_prefix(&db, q, skip, limit);
        }
    }
}

#[test]
fn limit_does_not_change_ordering_aggregation_or_filters() {
    let db = Database::in_memory();
    let one = |q: &str| rows(&run(&db, q));
    assert_eq!(one("UNWIND [3, 1, 2] AS x RETURN x ORDER BY x LIMIT 1"), vec!["1"]);
    assert_eq!(one("UNWIND [1, 1, 2, 3] AS x RETURN DISTINCT x LIMIT 2"), vec!["1", "2"]);
    assert_eq!(one("UNWIND range(1, 10) AS x RETURN count(x) LIMIT 1"), vec!["10"]);
    assert_eq!(one("UNWIND range(1, 10) AS x WITH x LIMIT 3 RETURN sum(x)"), vec!["6"]);
    assert_eq!(
        one("UNWIND range(1, 10) AS x WITH x LIMIT 5 WHERE x > 3 RETURN x"),
        vec!["4", "5"]
    );
    assert_eq!(
        one("UNWIND range(1, 10) AS x WITH x WHERE x > 3 RETURN x LIMIT 2"),
        vec!["4", "5"]
    );
    assert_eq!(
        one("UNWIND range(1, 10) AS x WITH x SKIP 2 LIMIT 4 RETURN x SKIP 1 LIMIT 2"),
        vec!["4", "5"]
    );
    let r = db
        .execute_with_params("UNWIND [1, 2, 3] AS x RETURN x SKIP $s LIMIT $n", &params! { "s" => 1, "n" => 1 })
        .unwrap();
    assert_eq!(rows(&r), vec!["2"]);
}

#[test]
fn rows_past_the_limit_are_not_evaluated() {
    let db = Database::in_memory();
    // The second row would divide by zero.
    assert_eq!(rows(&run(&db, "UNWIND [1, 0] AS d RETURN 10 / d LIMIT 1")), vec!["10"]);
    assert!(db.execute("UNWIND [1, 0] AS d RETURN 10 / d LIMIT 2").is_err());
    assert!(db.execute("UNWIND [1, 0] AS d RETURN 10 / d ORDER BY d LIMIT 1").is_err());
}

#[test]
fn limit_never_skips_updates() {
    let db = db_with("UNWIND range(1, 5) AS i CREATE (:T {i: i})");
    assert_eq!(rows(&run(&db, "MATCH (n:T) SET n.x = 1 RETURN n.i LIMIT 1")).len(), 1);
    assert_eq!(scalar(&db, "MATCH (n:T) WHERE n.x = 1 RETURN count(n)").to_string(), "5");
    assert_eq!(
        rows(&run(&db, "UNWIND range(1, 4) AS i CREATE (c:C) RETURN i LIMIT 2")),
        vec!["1", "2"]
    );
    assert_eq!(scalar(&db, "MATCH (c:C) RETURN count(c)").to_string(), "4");
    // A LIMIT before the update does restrict it.
    run(&db, "MATCH (n:T) WITH n LIMIT 2 SET n.y = 1");
    assert_eq!(scalar(&db, "MATCH (n:T) WHERE n.y = 1 RETURN count(n)").to_string(), "2");
    assert!(db.execute("MATCH (n:T) RETURN n LIMIT -1").is_err());
}

// ---- Top-N ------------------------------------------------------------------------------

#[test]
fn top_n_equals_the_leading_rows_of_a_full_sort() {
    // Enough rows (and ties) that the Top-N buffer is cut back several times.
    let db = db_with("UNWIND range(1, 3000) AS i CREATE (:N {i: i, a: i % 7, b: (i * 37) % 11})");
    let queries = [
        "MATCH (n:N) RETURN n.i AS i ORDER BY n.a",
        "MATCH (n:N) RETURN n.i AS i ORDER BY n.a DESC, n.b",
        "MATCH (n:N) RETURN n.i, n.a AS a ORDER BY a, n.i DESC",
        "MATCH (n:N) RETURN DISTINCT n.a AS a, n.b AS b ORDER BY b DESC, a",
        "MATCH (n:N) RETURN n.a AS a, count(*) AS c ORDER BY c DESC, a",
        "MATCH (n:N) WITH n.b AS b, n ORDER BY n.a LIMIT 50 RETURN b, n.i AS i",
        "UNWIND range(1, 2000) AS x RETURN x % 3 AS m ORDER BY m",
    ];
    for q in queries {
        for (skip, limit) in [(0, 0), (0, 1), (0, 10), (5, 7), (0, 600), (100, 1500), (2990, 50)] {
            assert_prefix(&db, q, skip, limit);
        }
    }
}

#[test]
fn top_n_plan_and_errors() {
    let db = Database::in_memory();
    let plan = db.explain("UNWIND [3, 1, 2] AS x RETURN x ORDER BY x DESC SKIP 1 LIMIT 1").unwrap();
    assert!(plan.contains("Top 1 + 1 by x DESC"), "{plan}");
    let plan = db.explain("UNWIND [3, 1, 2] AS x RETURN x ORDER BY x").unwrap();
    assert!(plan.contains("Sort x") && !plan.contains("Top"), "{plan}");
    assert_eq!(
        rows(&run(&db, "UNWIND [3, 1, 2] AS x RETURN x ORDER BY x DESC SKIP 1 LIMIT 1")),
        vec!["2"]
    );
    let r = db
        .execute_with_params("UNWIND [3, 1, 2] AS x RETURN x ORDER BY x LIMIT $n", &params! { "n" => 2 })
        .unwrap();
    assert_eq!(rows(&r), vec!["1", "2"]);
    assert!(db.execute("UNWIND [1] AS x RETURN x ORDER BY x LIMIT -1").is_err());
    assert!(db.execute("UNWIND [1] AS x RETURN x ORDER BY x SKIP -1 LIMIT 1").is_err());
    assert!(db.execute("UNWIND [1] AS x RETURN x ORDER BY count(x) LIMIT 1").is_err());
}

// ---- EXPLAIN / PROFILE ------------------------------------------------------------------

/// A plan line's operator text and measured rows, for compact assertions.
fn plan_rows(r: &QueryResult) -> Vec<(String, Option<u64>)> {
    r.plan
        .as_ref()
        .expect("a plan")
        .lines
        .iter()
        .map(|l| (format!("{}{}", "  ".repeat(l.depth), l.operator), l.rows))
        .collect()
}

#[test]
fn explain_plans_without_running() {
    let db = db_with(SOCIAL);
    let r = run(&db, "EXPLAIN MATCH (a:Person)-[:KNOWS]->(b) RETURN b.name ORDER BY b.name LIMIT 2");
    assert!(r.rows.is_empty() && r.columns.is_empty());
    let plan = r.plan.unwrap();
    assert!(!plan.profiled);
    assert_eq!(plan.to_string(), db.explain("MATCH (a:Person)-[:KNOWS]->(b) RETURN b.name ORDER BY b.name LIMIT 2").unwrap());
    assert!(plan.lines.iter().all(|l| l.rows.is_none() && l.time.is_none()));
    assert!(plan.lines.iter().any(|l| l.estimated_rows.is_some()));
    // Writes are planned, not applied.
    let r = run(&db, "explain CREATE (:Ghost)");
    assert_eq!(r.plan.unwrap().lines[0].operator, "Create (:Ghost)");
    assert_eq!(scalar(&db, "MATCH (g:Ghost) RETURN count(g)").to_string(), "0");
    assert!(run(&db, "MATCH (n) RETURN n").plan.is_none());
}

#[test]
fn profile_counts_rows_per_operator() {
    let db = db_with(SOCIAL);
    let r = run(&db, "PROFILE MATCH (a:Person)-[:KNOWS]->(b) RETURN b.name ORDER BY b.name LIMIT 2");
    assert_eq!(rows(&r), vec!["\"Bob\"", "\"Cat\""]);
    assert_eq!(
        plan_rows(&r),
        [
            ("Select", 2),
            ("  Limit 2", 2),
            ("    Top 2 by b.name", 2),
            ("      Project b.name", 3),
            ("        Expand(All) (a)-[:KNOWS]->(b)", 3),
            ("          NodeByLabelScan (a:Person)", 4),
            ("            Start", 1),
        ]
        .map(|(o, n)| (o.to_string(), Some(n)))
    );
    let plan = r.plan.unwrap();
    assert!(plan.profiled && plan.lines.iter().all(|l| l.time.is_some()));
    assert!(plan.to_string().contains("Start [1 row, "), "{plan}");

    // A LIMIT stops the scan early, and the plan shows it.
    let r = run(&db, "PROFILE MATCH (a:Person) RETURN a LIMIT 1");
    assert_eq!(plan_rows(&r)[3], ("      NodeByLabelScan (a:Person)".to_string(), Some(1)));

    // Optional matches count each input row's matches; their Argument line is not measured.
    let r = run(&db, "PROFILE MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name");
    let lines = plan_rows(&r);
    assert!(lines.contains(&("    Optional".to_string(), Some(4))), "{lines:?}");
    assert!(lines.contains(&("      Expand(All) (a)-[:KNOWS]->(b)".to_string(), Some(3))), "{lines:?}");
    assert!(lines.contains(&("          Argument".to_string(), None)), "{lines:?}");

    // Joins: the build side is measured once.
    let r = run(&db, "PROFILE MATCH (a:Person), (c:Company) RETURN count(*)");
    let lines = plan_rows(&r);
    assert!(lines.contains(&("    CartesianProduct".to_string(), Some(4))), "{lines:?}");
    assert!(lines.iter().filter(|(o, _)| o.contains("NodeByLabelScan")).all(|(_, n)| n.is_some()));
}

#[test]
fn profile_runs_updates() {
    let db = db_with(SOCIAL);
    let r = run(&db, "PROFILE MATCH (p:Person) WHERE p.age > 30 SET p.old = true RETURN count(p)");
    assert_eq!(rows(&r), vec!["2"]);
    assert_eq!(r.stats.properties_set, 2);
    let lines = plan_rows(&r);
    assert!(lines.iter().any(|(o, n)| o.trim() == "Set (1 items)" && *n == Some(2)), "{lines:?}");
    assert_eq!(scalar(&db, "MATCH (p:Person {old: true}) RETURN count(p)").to_string(), "2");
    // A failed profiled write rolls back like any other.
    assert!(db.execute("PROFILE MATCH (p:Person) SET p.x = 1 / 0").is_err());
    assert_eq!(scalar(&db, "MATCH (p:Person) WHERE p.x IS NOT NULL RETURN count(p)").to_string(), "0");
}

#[test]
fn explain_and_profile_parse_errors() {
    let db = Database::in_memory();
    assert!(db.execute("EXPLAIN").is_err());
    assert!(db.execute("PROFILE EXPLAIN RETURN 1").is_err());
    let mgr = graphbuff::DatabaseManager::in_memory();
    let mut mgr_session = mgr.session();
    assert!(mgr_session.execute("EXPLAIN SHOW DATABASES").is_err());
    assert!(mgr_session.execute("PROFILE USE graphbuff").is_err());
    assert!(mgr_session.execute("PROFILE USE graphbuff RETURN 1 AS x").unwrap().plan.is_some());
    // Not keywords elsewhere.
    assert_eq!(rows(&run(&db, "WITH 1 AS explain, 2 AS profile RETURN explain + profile")), vec!["3"]);
}

// ---- shortest paths ---------------------------------------------------------------------

/// a -> b -> c -> d, a -> e -> d, a -> f -> d (the two shortest), d -[:S]-> a; z alone.
const ROUTES: &str = "
    CREATE (a:N {n: 'a'}), (b:N {n: 'b'}), (c:N {n: 'c'}), (d:N {n: 'd'}),
           (e:N {n: 'e'}), (f:N {n: 'f'}), (z:N {n: 'z'}),
           (a)-[:R {w: 1}]->(b), (b)-[:R {w: 1}]->(c), (c)-[:R {w: 1}]->(d),
           (a)-[:R {w: 2}]->(e), (e)-[:R {w: 2}]->(d),
           (a)-[:R {w: 1}]->(f), (f)-[:R {w: 2}]->(d),
           (d)-[:S]->(a);
";

/// The rows of `q`, sorted.
fn sorted_rows(db: &Database, q: &str) -> Vec<String> {
    let mut out = rows(&run(db, q));
    out.sort();
    out
}

#[test]
fn shortest_path() {
    let db = db_with(ROUTES);
    let q = |rest: &str| format!("MATCH (a:N {{n: 'a'}}), (d:N {{n: 'd'}}) {rest}");
    assert_eq!(
        scalar(&db, &q("MATCH p = shortestPath((a)-[:R*]->(d)) RETURN length(p)")).to_string(),
        "2"
    );
    assert_eq!(
        sorted_rows(&db, &q("MATCH p = allShortestPaths((a)-[:R*]->(d)) RETURN [x IN nodes(p) | x.n]")),
        vec!["[\"a\", \"e\", \"d\"]", "[\"a\", \"f\", \"d\"]"]
    );
    // In the same MATCH as the endpoints, and with endpoints the pattern finds itself.
    assert_eq!(
        scalar(&db, "MATCH (a:N {n: 'a'}), (d:N {n: 'd'}), p = shortestPath((a)-[:R*]->(d)) RETURN length(p)")
            .to_string(),
        "2"
    );
    assert_eq!(
        scalar(&db, "MATCH p = shortestPath((:N {n: 'a'})-[:R*]->(:N {n: 'd'})) RETURN length(p)").to_string(),
        "2"
    );
    // The relationship variable holds a list in path order.
    assert_eq!(
        sorted_rows(&db, &q("MATCH allShortestPaths((a)-[rs:R*]->(d)) RETURN [r IN rs | r.w]")),
        vec!["[1, 2]", "[2, 2]"]
    );
    // Direction and types.
    assert!(run(&db, &q("MATCH p = shortestPath((d)-[:R*]->(a)) RETURN p")).rows.is_empty());
    assert_eq!(scalar(&db, &q("MATCH p = shortestPath((d)-[*]->(a)) RETURN length(p)")).to_string(), "1");
    assert_eq!(scalar(&db, &q("MATCH p = shortestPath((d)-[:R*]-(a)) RETURN length(p)")).to_string(), "2");
    assert_eq!(scalar(&db, &q("MATCH p = shortestPath((d)<-[:R|S*]-(a)) RETURN length(p)")).to_string(), "2");
    assert_eq!(
        sorted_rows(&db, &q("MATCH p = allShortestPaths((d)-[:R|S*]-(a)) RETURN [x IN nodes(p) | x.n]")),
        vec!["[\"d\", \"a\"]"]
    );
    // Length bounds, and a single hop without `*`.
    assert!(run(&db, &q("MATCH p = shortestPath((a)-[:R*..1]->(d)) RETURN p")).rows.is_empty());
    assert!(run(&db, &q("MATCH p = shortestPath((a)-[:R]->(d)) RETURN p")).rows.is_empty());
    assert_eq!(
        rows(&run(&db, &q("MATCH p = shortestPath((d)-[r:S]->(a)) RETURN type(r), length(p)"))),
        vec!["\"S\", 1"]
    );
    // From a node to itself: only the empty path, and only with a minimum of 0.
    assert_eq!(
        scalar(&db, "MATCH (a:N {n: 'a'}) MATCH p = shortestPath((a)-[*0..]->(a)) RETURN length(p)").to_string(),
        "0"
    );
    assert!(run(&db, "MATCH (a:N {n: 'a'}) MATCH p = shortestPath((a)-[*]->(a)) RETURN p").rows.is_empty());
    // Relationship properties restrict the search.
    assert_eq!(
        sorted_rows(&db, &q("MATCH p = allShortestPaths((a)-[:R* {w: 1}]->(d)) RETURN [x IN nodes(p) | x.n]")),
        vec!["[\"a\", \"b\", \"c\", \"d\"]"]
    );
    assert_eq!(
        sorted_rows(
            &db,
            "MATCH (a:N {n: 'a'}), (c:N {n: 'c'}) MATCH p = allShortestPaths((a)-[:R* {w: 1}]->(c)) \
             RETURN [x IN nodes(p) | x.n]"
        ),
        vec!["[\"a\", \"b\", \"c\"]"]
    );
    // Unreachable, and under OPTIONAL MATCH.
    assert!(
        run(&db, "MATCH (a:N {n: 'a'}), (z:N {n: 'z'}) MATCH p = shortestPath((a)-[*]-(z)) RETURN p")
            .rows
            .is_empty()
    );
    assert_eq!(
        rows(&run(
            &db,
            "MATCH (a:N {n: 'a'}), (z:N {n: 'z'}) OPTIONAL MATCH p = shortestPath((a)-[*]-(z)) RETURN a.n, p"
        )),
        vec!["\"a\", null"]
    );
    // Once per input row.
    assert_eq!(
        rows(&run(
            &db,
            "MATCH (x:N) WHERE x.n IN ['b', 'e', 'z'] \
             OPTIONAL MATCH p = shortestPath((x)-[:R*]->(:N {n: 'd'})) RETURN x.n, length(p) ORDER BY x.n"
        )),
        vec!["\"b\", 2", "\"e\", 1", "\"z\", null"]
    );
}

#[test]
fn shortest_path_predicates_apply_during_the_search() {
    let db = db_with(ROUTES);
    let q = |rest: &str| format!("MATCH (a:N {{n: 'a'}}), (d:N {{n: 'd'}}) {rest}");
    // The shortest paths go through e or f; excluding both finds the longer one.
    let avoid = q("MATCH p = shortestPath((a)-[:R*]->(d)) WHERE none(x IN nodes(p) WHERE x.n IN ['e', 'f']) \
                   RETURN [x IN nodes(p) | x.n]");
    assert_eq!(scalar(&db, &avoid).to_string(), "[\"a\", \"b\", \"c\", \"d\"]");
    let plan = run(&db, &format!("EXPLAIN {avoid}")).plan.unwrap();
    assert!(
        plan.lines
            .iter()
            .any(|l| l.operator.contains("ShortestPath p = (a)-[:R*1..]->(d) with predicates")),
        "{plan:?}"
    );
    assert_eq!(
        sorted_rows(&db, &q("MATCH p = allShortestPaths((a)-[rs:R*]->(d)) WHERE all(r IN rs WHERE r.w = 2) \
                             RETURN [x IN nodes(p) | x.n]")),
        vec!["[\"a\", \"e\", \"d\"]"]
    );
    assert_eq!(
        sorted_rows(&db, &q("MATCH p = allShortestPaths((a)-[:R*]->(d)) WHERE length(p) > 2 RETURN length(p)")),
        vec!["3"]
    );
    assert!(run(&db, &q("MATCH p = shortestPath((a)-[:R*]->(d)) WHERE length(p) > 3 RETURN p")).rows.is_empty());
    // A conjunct on the endpoints alone is an ordinary filter.
    assert!(run(&db, &q("MATCH p = shortestPath((a)-[:R*]->(d)) WHERE a.n = 'x' RETURN p")).rows.is_empty());
}

#[test]
fn shortest_path_errors() {
    let db = db_with(ROUTES);
    let syntax = |q: &str| assert!(matches!(db.execute(q), Err(Error::Syntax { .. })), "{q}");
    syntax("MATCH p = shortestPath((a)-[*]->(b)-[*]->(c)) RETURN p");
    syntax("MATCH p = shortestPath((a)) RETURN p");
    syntax("MATCH p = shortestPath((a)-[*2..]->(b)) RETURN p");
    syntax("MATCH p = shortestPath((a)-[*]->(b) RETURN p");
    let semantic = |q: &str| assert!(matches!(db.execute(q), Err(Error::Semantic(_))), "{q}");
    semantic("CREATE p = shortestPath((a)-[:R]->(b))");
    semantic("MERGE p = shortestPath((a)-[:R]->(b))");
    semantic("MATCH ()-[r]->() MATCH p = shortestPath((a)-[r*]->(b)) RETURN p");
}

#[test]
fn shortest_paths_equal_the_shortest_variable_length_matches() {
    // A random graph: for every pair of nodes, allShortestPaths finds exactly the
    // variable-length matches of the smallest length, and shortestPath one of them.
    let mut seed: u64 = 0x5eed;
    let mut next = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let nodes = 12;
    let mut setup = format!("UNWIND range(0, {}) AS i CREATE (:V {{i: i}});", nodes - 1);
    for _ in 0..24 {
        let (a, b, t) = (next(nodes), next(nodes), if next(4) == 0 { "S" } else { "R" });
        setup.push_str(&format!("MATCH (a:V {{i: {a}}}), (b:V {{i: {b}}}) CREATE (a)-[:{t}]->(b);"));
    }
    let db = db_with(&setup);
    let mut found_some = 0;
    for pattern in ["-[:R*..5]->", "-[*..4]-", "<-[:R|S*..8]-", "-[:S*0..3]-"] {
        for a in 0..nodes {
            for b in 0..nodes {
                let ends = format!("MATCH (a:V {{i: {a}}}), (b:V {{i: {b}}}) ");
                let expected = sorted_rows(
                    &db,
                    &format!(
                        "{ends}MATCH p = (a){pattern}(b) WITH p ORDER BY length(p) \
                         WITH collect(p) AS ps UNWIND ps AS p WITH p, ps WHERE length(p) = length(ps[0]) \
                         RETURN [r IN relationships(p) | id(r)]"
                    ),
                );
                let ids = "RETURN [r IN relationships(p) | id(r)]";
                let all = sorted_rows(&db, &format!("{ends}MATCH p = allShortestPaths((a){pattern}(b)) {ids}"));
                // A path from a node to itself is only shortest when it is empty.
                let expected = if a == b { expected.into_iter().filter(|r| r == "[]").collect() } else { expected };
                assert_eq!(all, expected, "allShortestPaths({a}){pattern}({b})");
                // The same through the search for paths that pass predicates.
                let filtered = sorted_rows(
                    &db,
                    &format!("{ends}MATCH p = allShortestPaths((a){pattern}(b)) WHERE length(p) >= 0 {ids}"),
                );
                assert_eq!(filtered, expected, "allShortestPaths({a}){pattern}({b}) WHERE ...");
                let one = rows(&run(&db, &format!("{ends}MATCH p = shortestPath((a){pattern}(b)) {ids}")));
                assert_eq!(one.len(), expected.len().min(1), "shortestPath({a}){pattern}({b})");
                assert!(one.iter().all(|r| expected.contains(r)), "shortestPath({a}){pattern}({b})");
                found_some += one.len();
            }
        }
    }
    assert!(found_some > 100, "{found_some}");
}

// ---- subqueries ---------------------------------------------------------------------------

#[test]
fn exists_subqueries() {
    let db = db_with(SOCIAL);
    let names = |q: &str| rows(&run(&db, q));
    // Short form: a pattern, with an optional WHERE.
    assert_eq!(
        names("MATCH (p:Person) WHERE EXISTS { (p)-[:KNOWS]->() } RETURN p.name ORDER BY p.name"),
        vec!["\"Ann\"", "\"Bob\"", "\"Cat\""]
    );
    assert_eq!(
        names("MATCH (p:Person) WHERE EXISTS { (p)-[:KNOWS]->(q) WHERE q.age > 30 } RETURN p.name"),
        vec!["\"Bob\""]
    );
    assert_eq!(
        names("MATCH (p:Person) WHERE NOT EXISTS { (p)-[:KNOWS]->() } RETURN p.name"),
        vec!["\"Dan\""]
    );
    // Full form, which can use WITH, UNWIND and anything else that reads.
    assert_eq!(
        names(
            "MATCH (p:Person) WHERE EXISTS { MATCH (p)-[:WORKS_AT]->(c) WITH c WHERE c.name = 'Acme' RETURN c } \
             RETURN p.name ORDER BY p.name"
        ),
        vec!["\"Ann\"", "\"Dan\""]
    );
    // As a value, and with outer aliases and comprehension variables.
    assert_eq!(
        names("MATCH (p:Person {name: 'Dan'}) RETURN EXISTS { (p)<-[:KNOWS]-() } AS a, EXISTS { (p)-[:KNOWS]->() } AS b"),
        vec!["true, false"]
    );
    assert_eq!(
        names("WITH 40 AS limit RETURN [x IN [20, 30, 40] WHERE EXISTS { MATCH (p:Person) WHERE p.age > x AND x < limit }] AS xs"),
        vec!["[20, 30]"]
    );
    // UNION inside.
    assert_eq!(
        names(
            "MATCH (p:Person) WHERE EXISTS { MATCH (p)-[:WORKS_AT]->(c) RETURN c UNION MATCH (p)<-[:KNOWS]-(c) RETURN c } \
             RETURN p.name ORDER BY p.name"
        ),
        vec!["\"Ann\"", "\"Bob\"", "\"Cat\"", "\"Dan\""]
    );
}

#[test]
fn count_and_collect_subqueries() {
    let db = db_with(SOCIAL);
    let names = |q: &str| rows(&run(&db, q));
    assert_eq!(
        names("MATCH (p:Person) RETURN p.name, COUNT { (p)-[]-() } AS degree ORDER BY p.name"),
        vec!["\"Ann\", 2", "\"Bob\", 2", "\"Cat\", 2", "\"Dan\", 2"]
    );
    assert_eq!(
        names("MATCH (c:Company) RETURN COUNT { MATCH (p)-[:WORKS_AT]->(c) RETURN p } AS staff"),
        vec!["2"]
    );
    assert_eq!(
        names("MATCH (p:Person) WHERE COUNT { (p)-[:KNOWS]-() } > 1 RETURN p.name ORDER BY p.name"),
        vec!["\"Bob\"", "\"Cat\""]
    );
    // UNION removes duplicates, UNION ALL keeps them.
    let union = |all: &str| {
        names(&format!(
            "MATCH (c:Company) RETURN COUNT {{ MATCH (p)-[:WORKS_AT]->(c) RETURN p UNION {all} \
             MATCH (p:Person {{name: 'Ann'}}) RETURN p }} AS n"
        ))
    };
    assert_eq!(union(""), vec!["2"]);
    assert_eq!(union("ALL"), vec!["3"]);
    assert_eq!(
        names(
            "MATCH (p:Person {name: 'Bob'}) \
             RETURN COLLECT { MATCH (p)-[:KNOWS]-(q) RETURN q.name ORDER BY q.name DESC } AS friends"
        ),
        vec!["[\"Cat\", \"Ann\"]"]
    );
    // Nested, and in ORDER BY.
    assert_eq!(
        names(
            "MATCH (p:Person) RETURN p.name ORDER BY COUNT { MATCH (p)-[:KNOWS]->(q) WHERE EXISTS { (q)-[:WORKS_AT]->() } } DESC, \
             p.name LIMIT 1"
        ),
        vec!["\"Cat\""]
    );
}

#[test]
fn subquery_expression_errors() {
    let db = db_with(SOCIAL);
    let semantic = |q: &str| assert!(matches!(db.execute(q), Err(Error::Semantic(_))), "{q}");
    semantic("MATCH (p:Person) WHERE EXISTS { CREATE (x:X) } RETURN p");
    semantic("RETURN COUNT { MATCH (p)-->(q) SET q.x = 1 RETURN q } AS n");
    semantic("RETURN COLLECT { MATCH (p:Person) RETURN p.name, p.age } AS xs");
    semantic("RETURN COLLECT { MATCH (p:Person) } AS xs");
    assert!(matches!(db.execute("RETURN COLLECT { (p:Person) } AS xs"), Err(Error::Syntax { .. })));
    assert!(matches!(db.execute("RETURN EXISTS { MATCH (p) RETURN p AS xs"), Err(Error::Syntax { .. })));
    assert_eq!(db.execute("MATCH (n:Nothing) RETURN 1").unwrap().stats.nodes_created, 0);
}

#[test]
fn call_subqueries() {
    let db = db_with(SOCIAL);
    let names = |q: &str| rows(&run(&db, q));
    // Uncorrelated: runs once per row, returns its columns.
    assert_eq!(
        names("CALL { MATCH (p:Person) RETURN count(p) AS n } RETURN n"),
        vec!["4"]
    );
    assert_eq!(
        names("UNWIND [1, 2] AS i CALL { MATCH (p:Person) RETURN max(p.age) AS oldest } RETURN i, oldest"),
        vec!["1, 41", "2, 41"]
    );
    // Importing WITH, and the scope clause forms. Rows without results are dropped.
    let friends = vec!["\"Ann\", \"Bob\"", "\"Bob\", \"Cat\"", "\"Cat\", \"Dan\""];
    assert_eq!(
        names(
            "MATCH (p:Person) CALL { WITH p MATCH (p)-[:KNOWS]->(q) RETURN q.name AS friend } \
             RETURN p.name, friend ORDER BY p.name"
        ),
        friends
    );
    assert_eq!(
        names("MATCH (p:Person) CALL (p) { MATCH (p)-[:KNOWS]->(q) RETURN q.name AS friend } RETURN p.name, friend ORDER BY p.name"),
        friends
    );
    assert_eq!(
        names("MATCH (p:Person) CALL (*) { MATCH (p)-[:KNOWS]->(q) RETURN q.name AS friend } RETURN p.name, friend ORDER BY p.name"),
        friends
    );
    assert_eq!(
        names("CALL () { RETURN 1 AS one } RETURN one"),
        vec!["1"]
    );
    // Aggregation per row.
    assert_eq!(
        names("MATCH (p:Person) CALL (p) { MATCH (p)-[:KNOWS]->(q) RETURN count(q) AS c } RETURN p.name, c ORDER BY p.name"),
        vec!["\"Ann\", 1", "\"Bob\", 1", "\"Cat\", 1", "\"Dan\", 0"]
    );
    // Per-row top-N.
    assert_eq!(
        names(
            "MATCH (c:Company) CALL (c) { MATCH (p)-[:WORKS_AT]->(c) RETURN p.name AS youngest ORDER BY p.age LIMIT 1 } \
             RETURN c.name, youngest"
        ),
        vec!["\"Acme\", \"Dan\""]
    );
    // UNION.
    assert_eq!(
        names(
            "CALL { MATCH (p:Person) WHERE p.age > 35 RETURN p.name AS n UNION MATCH (c:Company) RETURN c.name AS n } \
             RETURN n ORDER BY n"
        ),
        vec!["\"Acme\"", "\"Cat\""]
    );
    // Streaming: a LIMIT above stops calling.
    assert_eq!(
        names("UNWIND range(1, 1000000) AS i CALL (i) { RETURN i * 2 AS d } RETURN d LIMIT 2"),
        vec!["2", "4"]
    );
    let plan = run(&db, "EXPLAIN MATCH (p:Person) CALL (p) { MATCH (p)-->(q) RETURN q } RETURN q").plan.unwrap();
    assert!(plan.lines.iter().any(|l| l.operator == "CallSubquery (p)"), "{plan:?}");
}

#[test]
fn call_subqueries_with_updates() {
    let db = db_with(SOCIAL);
    // A unit subquery keeps every input row once.
    let r = run(&db, "MATCH (p:Person) CALL (p) { CREATE (:Tag {of: p.name}) } RETURN count(*) AS n");
    assert_eq!(rows(&r), vec!["4"]);
    assert_eq!(r.stats.nodes_created, 4);
    // A query can end with one.
    let r = run(&db, "MATCH (p:Person) CALL (p) { SET p.seen = true }");
    assert_eq!(r.stats.properties_set, 4);
    assert!(r.rows.is_empty());
    // Returning rows from an update.
    assert_eq!(
        rows(&run(&db, "UNWIND [3, 1, 2] AS i CALL (i) { CREATE (t:T {i: i}) RETURN t } RETURN t.i ORDER BY t.i")),
        vec!["1", "2", "3"]
    );
    // Each run sees the changes of the runs before it.
    assert_eq!(
        rows(&run(
            &db,
            "UNWIND [1, 2, 3] AS i CALL (i) { MERGE (c:Counter) SET c.n = coalesce(c.n, 0) + i RETURN c.n AS n } RETURN n"
        )),
        vec!["1", "3", "6"]
    );
    // A failed run rolls back the whole query.
    let before = scalar(&db, "MATCH (n) RETURN count(n)").to_string();
    assert!(db.execute("UNWIND [1, 0] AS i CALL (i) { CREATE (:U {x: 1 / i}) }").is_err());
    assert_eq!(scalar(&db, "MATCH (n) RETURN count(n)").to_string(), before);
}

#[test]
fn call_subquery_errors() {
    let db = db_with(SOCIAL);
    let semantic = |q: &str| assert!(matches!(db.execute(q), Err(Error::Semantic(_))), "{q}");
    semantic("MATCH (p:Person) CALL (p) { RETURN 1 AS p } RETURN p");
    semantic("CALL (x) { RETURN 1 AS one } RETURN one");
    semantic("MATCH (p:Person) CALL { MATCH (q) RETURN q }");
    semantic("CALL { RETURN 1 AS a UNION RETURN 2 AS b } RETURN a");
    semantic("CALL { CREATE INDEX FOR (n:P) ON (n.x) } RETURN 1");
    // Without an import, outer variables are not visible.
    assert!(db.execute("MATCH (p:Person) CALL { MATCH (p)-->(q) RETURN q } RETURN q").is_ok());
    assert!(db.execute("MATCH (p:Person) CALL { RETURN p.name AS n } RETURN n").is_err());
    assert!(matches!(
        db.execute("UNWIND [1] AS i CALL (i) { CREATE (:X) } IN TRANSACTIONS"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(db.execute("CALL { RETURN 1 AS x RETURN x"), Err(Error::Syntax { .. })));
}

// ---- FOREACH, regex, LOAD CSV ---------------------------------------------------------------

#[test]
fn regex_match() {
    let db = db_with(SOCIAL);
    let names = |q: &str| rows(&run(&db, q));
    assert_eq!(
        names("MATCH (p:Person) WHERE p.name =~ '[AB].*' RETURN p.name ORDER BY p.name"),
        vec!["\"Ann\"", "\"Bob\""]
    );
    // The whole string must match; flags are inline.
    assert_eq!(names("RETURN 'Ann' =~ 'A' AS a, 'Ann' =~ 'A.*' AS b, 'ANN' =~ '(?i)ann' AS c"), vec!["false, true, true"]);
    assert_eq!(names("RETURN null =~ '.*' AS a, 'x' =~ null AS b, 1 =~ '1' AS c"), vec!["null, null, null"]);
    assert_eq!(
        names("WITH '\\\\d+' AS re UNWIND ['12', '1a', ''] AS s RETURN s, s =~ re ORDER BY s"),
        vec!["\"\", false", "\"12\", true", "\"1a\", false"]
    );
    assert_eq!(
        rows(&db.execute_with_params("RETURN 'a.b' =~ $re AS m", &params! { "re" => "a\\.b" }).unwrap()),
        vec!["true"]
    );
    assert!(matches!(db.execute("RETURN 'a' =~ '(a' AS m"), Err(Error::Runtime(_))));
    assert!(matches!(db.execute("RETURN 'a' =~ '(?=a)a' AS m"), Err(Error::Runtime(_))));
}

#[test]
fn foreach() {
    let db = db_with(SOCIAL);
    let r = run(&db, "FOREACH (name IN ['X', 'Y', 'Z'] | CREATE (:Tag {name: name}))");
    assert_eq!(r.stats.nodes_created, 3);
    assert!(r.rows.is_empty());
    // Per row, with outer variables; rows pass through.
    let r = run(
        &db,
        "MATCH (p:Person) WHERE p.age > 30 FOREACH (t IN ['a', 'b'] | MERGE (x:Tag {name: t}) CREATE (p)-[:HAS]->(x)) \
         RETURN p.name ORDER BY p.name",
    );
    assert_eq!(rows(&r), vec!["\"Ann\"", "\"Cat\""]);
    assert_eq!(r.stats.nodes_created, 2);
    assert_eq!(r.stats.relationships_created, 4);
    // SET, nested FOREACH, null lists, and each element seeing the earlier ones' changes.
    run(&db, "MATCH (p:Person) FOREACH (_ IN CASE WHEN p.age < 30 THEN [1] ELSE [] END | SET p:Young)");
    assert_eq!(
        rows(&run(&db, "MATCH (p:Young) RETURN p.name ORDER BY p.name")),
        vec!["\"Bob\"", "\"Dan\""]
    );
    run(&db, "CREATE (:Counter {n: 0}) WITH 1 AS one MATCH (c:Counter) FOREACH (i IN range(1, 3) | FOREACH (j IN [i, i] | SET c.n = c.n + j))");
    assert_eq!(scalar(&db, "MATCH (c:Counter) RETURN c.n").to_string(), "12");
    run(&db, "MATCH (p:Person {name: 'Ann'}) FOREACH (x IN null | SET p.never = true)");
    assert_eq!(scalar(&db, "MATCH (p:Person) WHERE p.never IS NOT NULL RETURN count(p)").to_string(), "0");
    // Paths: mark every node on a path.
    run(&db, "MATCH p = (:Person {name: 'Ann'})-[:KNOWS*]->(:Person {name: 'Dan'}) FOREACH (n IN nodes(p) | SET n.onPath = true)");
    assert_eq!(scalar(&db, "MATCH (p:Person {onPath: true}) RETURN count(p)").to_string(), "4");
    let plan = run(&db, "EXPLAIN MATCH (p:Person) FOREACH (x IN [1] | SET p.x = x)").plan.unwrap();
    assert!(plan.lines.iter().any(|l| l.operator == "Foreach x"), "{plan:?}");
    // Errors, and rollback.
    let semantic = |q: &str| assert!(matches!(db.execute(q), Err(Error::Semantic(_))), "{q}");
    semantic("FOREACH (x IN [1] | MATCH (n) SET n.x = 1)");
    semantic("FOREACH (x IN [1] | CREATE (:A) RETURN x)");
    semantic("WITH 1 AS x FOREACH (x IN [1] | CREATE (:A))");
    assert!(matches!(db.execute("FOREACH (x IN 5 | CREATE (:A))"), Err(Error::Type(_))));
    assert!(matches!(db.execute("FOREACH (x IN [1] CREATE (:A))"), Err(Error::Syntax { .. })));
    let before = scalar(&db, "MATCH (n) RETURN count(n)").to_string();
    assert!(db.execute("FOREACH (x IN [1, 0] | CREATE (:A {v: 1 / x}))").is_err());
    assert_eq!(scalar(&db, "MATCH (n) RETURN count(n)").to_string(), before);
}

/// A fresh directory for CSV files.
fn import_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graphbuff-import-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    dir
}

#[test]
fn load_csv() {
    let dir = import_dir("load");
    std::fs::write(dir.join("people.csv"), "name,age,city\nAnn,34,Prague\n\"Bob, Jr.\",25,\nCat,41,\"\"\n").unwrap();
    std::fs::write(dir.join("sub").join("plain.tsv"), "1\tx\n2\ty\n").unwrap();
    std::fs::write(dir.join("empty.csv"), "").unwrap();
    let db = Database::in_memory();
    // Off until an import directory is set.
    assert!(db.execute("LOAD CSV FROM 'file:///people.csv' AS row RETURN row").is_err());
    db.set_import_dir(Some(dir.clone()));

    assert_eq!(
        rows(&run(&db, "LOAD CSV WITH HEADERS FROM 'file:///people.csv' AS row RETURN row.name, row.age, row.city")),
        vec!["\"Ann\", \"34\", \"Prague\"", "\"Bob, Jr.\", \"25\", null", "\"Cat\", \"41\", \"\""]
    );
    assert_eq!(
        rows(&run(&db, "LOAD CSV FROM 'people.csv' AS row RETURN row LIMIT 1")),
        vec!["[\"name\", \"age\", \"city\"]"]
    );
    assert_eq!(
        rows(&run(&db, "LOAD CSV FROM 'file:///sub/plain.tsv' AS r FIELDTERMINATOR '\\t' RETURN toInteger(r[0]) + 1, r[1]")),
        vec!["2, \"x\"", "3, \"y\""]
    );
    assert!(run(&db, "LOAD CSV WITH HEADERS FROM 'empty.csv' AS row RETURN row").rows.is_empty());
    // Importing: create nodes, then relate them by a second file.
    let r = run(
        &db,
        "LOAD CSV WITH HEADERS FROM 'file:///people.csv' AS row \
         CREATE (:P {name: row.name, age: toInteger(row.age)})",
    );
    assert_eq!(r.stats.nodes_created, 3);
    assert_eq!(scalar(&db, "MATCH (p:P) RETURN sum(p.age)").to_string(), "100");
    std::fs::write(dir.join("knows.csv"), "a;b\nAnn;Cat\nCat;Bob, Jr.\n").unwrap();
    let r = run(
        &db,
        "LOAD CSV WITH HEADERS FROM 'knows.csv' AS row FIELDTERMINATOR ';' \
         MATCH (a:P {name: row.a}), (b:P {name: row.b}) CREATE (a)-[:KNOWS]->(b)",
    );
    assert_eq!(r.stats.relationships_created, 2);
    // The URL can come from the row.
    assert_eq!(
        rows(&run(&db, "UNWIND ['people.csv', 'knows.csv'] AS f LOAD CSV FROM f AS row RETURN f, count(*) ORDER BY f")),
        vec!["\"knows.csv\", 3", "\"people.csv\", 4"]
    );
    let plan = run(&db, "EXPLAIN LOAD CSV WITH HEADERS FROM 'x.csv' AS row RETURN row").plan.unwrap();
    assert!(plan.lines.iter().any(|l| l.operator == "LoadCsv WITH HEADERS AS row"), "{plan:?}");

    // Errors.
    for q in [
        "LOAD CSV FROM 'missing.csv' AS row RETURN row",
        "LOAD CSV FROM 'file:///../people.csv' AS row RETURN row",
        "LOAD CSV FROM 'sub/../../people.csv' AS row RETURN row",
        "LOAD CSV FROM 1 AS row RETURN row",
    ] {
        assert!(db.execute(q).is_err(), "{q}");
    }
    assert!(matches!(
        db.execute("LOAD CSV FROM 'https://example.com/x.csv' AS row RETURN row"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        db.execute("LOAD CSV FROM 'people.csv' AS row FIELDTERMINATOR ';;' RETURN row"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.execute("WITH 1 AS row LOAD CSV FROM 'people.csv' AS row RETURN row"),
        Err(Error::Semantic(_))
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn load_csv_through_the_database_manager() {
    let dir = import_dir("mgr");
    std::fs::write(dir.join("n.csv"), "1\n2\n").unwrap();
    let mgr = graphbuff::DatabaseManager::in_memory();
    let mut s = mgr.session();
    assert!(s.execute("LOAD CSV FROM 'n.csv' AS r RETURN r").is_err());
    mgr.set_import_dir(Some(dir.clone()));
    assert_eq!(rows(&s.execute("LOAD CSV FROM 'n.csv' AS r RETURN r[0]").unwrap()), vec!["\"1\"", "\"2\""]);
    // Databases created later can read too.
    s.execute("CREATE DATABASE other").unwrap();
    assert_eq!(rows(&s.execute("USE other LOAD CSV FROM 'n.csv' AS r RETURN count(r)").unwrap()), vec!["2"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- CALL procedures ----------------------------------------------------------------------

#[test]
fn builtin_schema_procedures() {
    let db = db_with(SOCIAL);
    // Standalone: every output, under its own name.
    let r = run(&db, "CALL db.labels()");
    assert_eq!(r.columns, ["label"]);
    assert_eq!(rows(&r), ["\"Admin\"", "\"Company\"", "\"Person\""]);
    // Without parentheses, too.
    assert_eq!(rows(&run(&db, "CALL db.labels")), rows(&r));
    let r = run(&db, "CALL db.relationshipTypes()");
    assert_eq!(r.columns, ["relationshipType"]);
    assert!(rows(&r).contains(&"\"KNOWS\"".to_string()));
    let keys = rows(&run(&db, "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey"));
    for k in ["\"age\"", "\"name\"", "\"since\""] {
        assert!(keys.contains(&k.to_string()), "{keys:?}");
    }
    // Labels and types that no longer have entities are not listed.
    run(&db, "MATCH (c:Company) DETACH DELETE c");
    assert_eq!(rows(&run(&db, "CALL db.labels()")), ["\"Admin\"", "\"Person\""]);
}

#[test]
fn procedure_yield_where_and_pipelines() {
    let db = db_with(SOCIAL);
    // YIELD renames, WHERE filters, later clauses use the yielded variables.
    let r = run(
        &db,
        "CALL db.labels() YIELD label AS l WHERE l STARTS WITH 'P' \
         MATCH (n) WHERE l IN labels(n) RETURN l, count(n) AS c",
    );
    assert_eq!(rows(&r), ["\"Person\", 4"]);
    // Per input row.
    let r = run(
        &db,
        "UNWIND [1, 2] AS i CALL db.labels() YIELD label RETURN i, count(label) AS n ORDER BY i",
    );
    assert_eq!(rows(&r), ["1, 3", "2, 3"]);
    // Standalone with YIELD: only the yielded fields, in YIELD order.
    run(&db, "CREATE INDEX person_name FOR (p:Person) ON (p.name)");
    let r = run(&db, "CALL db.indexes() YIELD properties, name");
    assert_eq!(r.columns, ["properties", "name"]);
    assert_eq!(rows(&r), ["[\"name\"], \"person_name\""]);
    let r = run(&db, "CALL db.indexes() YIELD *");
    assert_eq!(r.columns, ["name", "entityType", "labelOrType", "properties"]);
    // A VOID procedure keeps the rows; standalone it returns none.
    assert_eq!(scalar(&db, "UNWIND [1, 2, 3] AS x CALL db.awaitIndexes(10) RETURN sum(x)").as_int(), Some(6));
    let r = run(&db, "CALL db.awaitIndexes()");
    assert!(r.columns.is_empty() && r.rows.is_empty());
    // In a UNION.
    let r = run(&db, "CALL db.labels() YIELD label RETURN label AS x UNION RETURN 'extra' AS x");
    assert_eq!(r.rows.len(), 4);
    let plan = db.explain("CALL db.labels() YIELD label AS l WHERE l <> 'x' RETURN l").unwrap();
    assert!(plan.contains("ProcedureCall db.labels() YIELD label AS l"), "{plan}");
}

#[test]
fn procedure_errors() {
    let db = db_with(SOCIAL);
    let semantic = |q: &str| match db.execute(q) {
        Err(Error::Semantic(m)) => m,
        other => panic!("`{q}`: expected a semantic error, got {other:?}"),
    };
    assert!(semantic("CALL db.Labels()").contains("did you mean `db.labels`"));
    assert!(semantic("CALL db.labels(1)").contains("takes 0 argument"));
    assert!(semantic("CALL db.labels() YIELD nope").contains("no output `nope`"));
    assert!(semantic("MATCH (n) CALL db.labels() RETURN n").contains("must use YIELD"));
    assert!(semantic("MATCH (n) CALL db.labels YIELD label RETURN n").contains("parentheses"));
    assert!(semantic("MATCH (n) CALL db.labels() YIELD * RETURN n").contains("YIELD *"));
    assert!(semantic("UNWIND [1] AS label CALL db.labels() YIELD label RETURN label").contains("already declared"));
    assert!(semantic("CALL db.awaitIndexes() YIELD x").contains("VOID"));
    assert!(semantic("MATCH (n) CALL db.labels() YIELD label").contains("cannot conclude"));
}

#[test]
fn registered_procedures() {
    use graphbuff::{ProcedureSignature, Value};
    let db = db_with(SOCIAL);
    db.register_procedure(
        ProcedureSignature::new("test.older", ["age"], ["name", "age"])
            .optional("label", "Person")
            .description("People older than age"),
        |ctx, args| {
            let (Value::Int(min), Value::String(label)) = (&args[0], &args[1]) else {
                return Err(Error::Type("test.older(age :: INTEGER, label :: STRING)".into()));
            };
            let store = ctx.store();
            let mut out = Vec::new();
            for id in store.nodes_with_label(label) {
                let id = id?;
                if let Some(Value::Int(a)) = store.node_property(id, "age")?
                    && a > *min
                {
                    out.push(vec![store.node_property(id, "name")?.unwrap_or(Value::Null), Value::Int(a)]);
                }
            }
            Ok(out)
        },
    )
    .unwrap();
    let r = run(&db, "CALL test.older(30) YIELD name RETURN name ORDER BY name");
    assert_eq!(rows(&r), ["\"Ann\"", "\"Cat\""]);
    // Arguments are expressions over the row.
    let r = run(
        &db,
        "MATCH (p:Person {name: 'Bob'}) CALL test.older(p.age + 10) YIELD name, age RETURN name, age",
    );
    assert_eq!(rows(&r), ["\"Cat\", 41"]);
    // Implicit arguments come from parameters; the default fills the rest.
    let r = db
        .execute_with_params("CALL test.older", &params! { "age" => 35 })
        .unwrap();
    assert_eq!(rows(&r), ["\"Cat\", 41"]);
    assert!(db.execute("CALL test.older").is_err(), "a missing parameter fails");
    // The procedure's own errors come through.
    assert!(matches!(db.execute("CALL test.older('x')"), Err(Error::Type(_))));
    // Listed with the built-ins.
    let r = run(&db, "SHOW PROCEDURES");
    assert_eq!(r.columns, ["name", "signature", "description"]);
    let listed = rows(&r);
    assert!(listed.iter().any(|l| l.starts_with("\"db.labels\", \"db.labels() :: (label)\"")), "{listed:?}");
    let sig = scalar(
        &db,
        "CALL dbms.procedures() YIELD name, signature WHERE name = 'test.older' RETURN signature",
    );
    assert_eq!(sig.as_str(), Some("test.older(age, label = \"Person\") :: (name, age)"));
    let n = scalar(&db, "CALL dbms.procedures() YIELD name RETURN count(name)").as_int().unwrap();
    assert_eq!(n as usize, r.rows.len());
    // Names are unique and checked.
    let noop = |_: &graphbuff::ProcedureContext<'_>, _: &[Value]| Ok(Vec::new());
    assert!(db.register_procedure(ProcedureSignature::new("db.labels", [""; 0], ["x"]), noop).is_err());
    assert!(db.register_procedure(ProcedureSignature::new("bad name", [""; 0], ["x"]), noop).is_err());
    // A row of the wrong width is an error, not a panic.
    db.register_procedure(ProcedureSignature::new("test.bad", [""; 0], ["a", "b"]), |_, _| {
        Ok(vec![vec![Value::Int(1)]])
    })
    .unwrap();
    assert!(matches!(db.execute("CALL test.bad()"), Err(Error::Runtime(_))));
}

#[test]
fn procedures_through_the_database_manager() {
    use graphbuff::{DatabaseManager, ProcedureSignature, Value};
    let mgr = DatabaseManager::in_memory();
    mgr.register_procedure(ProcedureSignature::new("app.answer", [""; 0], ["value"]), |_, _| {
        Ok(vec![vec![Value::Int(42)]])
    })
    .unwrap();
    let mut session = mgr.session();
    session.execute("CREATE DATABASE other").unwrap();
    for db in ["graphbuff", "other"] {
        let r = session.execute(&format!("USE {db} CALL app.answer()")).unwrap();
        assert_eq!(r.get(0, "value").and_then(|v| v.as_int()), Some(42), "in {db}");
    }
    let r = session.execute("CALL dbms.components() YIELD name RETURN name").unwrap();
    assert_eq!(r.get(0, "name").and_then(|v| v.as_str()), Some("graphbuff"));
}

// ---- more functions ---------------------------------------------------------------

/// Evaluates each `expr` with `RETURN` and compares its printed value.
fn check_exprs(db: &Database, cases: &[(&str, &str)]) {
    for (expr, want) in cases {
        let got = scalar(db, &format!("RETURN {expr} AS v")).to_string();
        assert_eq!(&got, want, "RETURN {expr}");
    }
}

#[test]
fn reduce_expression() {
    let db = db_with(SOCIAL);
    check_exprs(
        &db,
        &[
            ("reduce(s = 0, x IN [1, 2, 3] | s + x)", "6"),
            ("reduce(s = '', x IN ['a', 'b'] | s + x)", "\"ab\""),
            ("reduce(acc = [], x IN range(1, 3) | acc + [x * x])", "[1, 4, 9]"),
            ("reduce(s = 1, x IN [] | s * x)", "1"),
            ("reduce(s = 0, x IN null | s + x)", "null"),
            // Nested, and shadowing an outer name.
            ("reduce(s = 0, x IN [[1, 2], [3]] | s + reduce(t = 0, x IN x | t + x))", "6"),
        ],
    );
    // Outer variables are visible; works inside aggregation.
    let r = run(
        &db,
        "MATCH (p:Person) WITH collect(p.age) AS ages \
         RETURN reduce(total = 0, a IN ages | total + a) AS total, size(ages) AS n",
    );
    assert_eq!(rows(&r), ["129, 4"]);
    let r = run(
        &db,
        "MATCH path = (:Person {name: 'Ann'})-[:KNOWS*2]->() \
         RETURN reduce(names = '', n IN nodes(path) | names + n.name) AS names",
    );
    assert_eq!(rows(&r), ["\"AnnBobCat\""]);
    assert!(db.execute("RETURN reduce(s = 0, x IN 5 | s + x)").is_err());
}

#[test]
fn string_functions() {
    let db = Database::in_memory();
    check_exprs(
        &db,
        &[
            ("upper('abc')", "\"ABC\""),
            ("lower('AbC')", "\"abc\""),
            ("btrim('  x  ')", "\"x\""),
            ("trim('xxhixx', 'x')", "\"hi\""),
            ("ltrim('xxhixx', 'x')", "\"hixx\""),
            ("rtrim('xxhixx', 'x')", "\"xxhi\""),
            ("btrim('-+a+-', '+-')", "\"a\""),
            ("rtrim(null, 'x')", "null"),
            ("char_length('héllo')", "5"),
            ("character_length('')", "0"),
            ("toStringOrNull(12)", "\"12\""),
            ("toStringOrNull([1])", "null"),
        ],
    );
}

#[test]
fn unicode_normalization() {
    let db = Database::in_memory();
    check_exprs(
        &db,
        &[
            ("normalize('\\u212B') = '\\u00C5'", "true"),
            ("normalize('\\u00C5', NFD) = 'A\\u030A'", "true"),
            ("normalize('A\\u030A', nfc) = '\\u00C5'", "true"),
            ("normalize('\\uFE64', NFKC)", "\"<\""),
            ("normalize('\\uFE64', NFC) = '\\uFE64'", "true"),
            ("normalize('\\uFB01', NFKD)", "\"fi\""),
            ("normalize('abc', 'NFKD')", "\"abc\""),
            ("normalize(null)", "null"),
            ("normalize(null, NFD)", "null"),
            ("'\\u00C5' IS NORMALIZED", "true"),
            ("'\\u00C5' IS NFC NORMALIZED", "true"),
            ("'\\u00C5' IS NFD NORMALIZED", "false"),
            ("'\\u00C5' IS NOT NFD NORMALIZED", "true"),
            ("'\\u212B' IS NOT NORMALIZED", "true"),
            ("'\\uFE64' IS NFKC NORMALIZED", "false"),
            ("'abc' IS NFKD NORMALIZED AND 'x' IS NOT NULL", "true"),
            ("null IS NORMALIZED", "null"),
            ("null IS NOT NFKC NORMALIZED", "null"),
        ],
    );
    let rows = db
        .execute("WITH 'x' AS NFD RETURN normalize('\\u00C5', NFD) = 'A\\u030A' AS d, NFD")
        .unwrap();
    assert_eq!(rows.len(), 1);
    for bad in ["RETURN normalize(1)", "RETURN 1 IS NORMALIZED", "RETURN normalize('a', 'NFX')"] {
        assert!(db.execute(bad).is_err(), "{bad} succeeded");
    }
}

#[test]
fn list_and_conversion_functions() {
    let db = Database::in_memory();
    check_exprs(
        &db,
        &[
            ("toIntegerList(['1', 2.7, 'x', null, true])", "[1, 2, null, null, 1]"),
            ("toFloatList([1, '2.5', 'no'])", "[1.0, 2.5, null]"),
            ("toBooleanList(['true', 0, 'maybe'])", "[true, false, null]"),
            ("toStringList([1, 2.5, true, [1]])", "[\"1\", \"2.5\", \"true\", null]"),
            ("toIntegerList(null)", "null"),
            ("toIntegerOrNull('7')", "7"),
            ("toIntegerOrNull('seven')", "null"),
            ("toFloatOrNull('1.5')", "1.5"),
            ("toBooleanOrNull('FALSE')", "false"),
            ("nullIf(1, 1)", "null"),
            ("nullIf('a', 'b')", "\"a\""),
            ("coalesce(nullIf(0, 0), 5)", "5"),
        ],
    );
    assert!(db.execute("RETURN toIntegerList(5)").is_err());
}

#[test]
fn value_type_and_uuid() {
    let db = db_with("CREATE (:N)-[:R]->(:N)");
    check_exprs(
        &db,
        &[
            ("valueType(1)", "\"INTEGER NOT NULL\""),
            ("valueType(1.5)", "\"FLOAT NOT NULL\""),
            ("valueType('a')", "\"STRING NOT NULL\""),
            ("valueType(null)", "\"NULL\""),
            ("valueType(true)", "\"BOOLEAN NOT NULL\""),
            ("valueType({a: 1})", "\"MAP NOT NULL\""),
            ("valueType([1, 2])", "\"LIST<INTEGER NOT NULL> NOT NULL\""),
            ("valueType([])", "\"LIST<NOTHING> NOT NULL\""),
            ("valueType([1, null])", "\"LIST<INTEGER> NOT NULL\""),
            ("valueType([1, 'a'])", "\"LIST<INTEGER NOT NULL | STRING NOT NULL> NOT NULL\""),
            ("valueType(date('2020-01-01'))", "\"DATE NOT NULL\""),
            ("valueType(datetime('2020-01-01T00:00Z'))", "\"ZONED DATETIME NOT NULL\""),
            ("valueType(point({x: 1, y: 2}))", "\"POINT NOT NULL\""),
        ],
    );
    let r = run(&db, "MATCH p = (a)-[r]->(b) RETURN valueType(a), valueType(r), valueType(p)");
    assert_eq!(rows(&r), ["\"NODE NOT NULL\", \"RELATIONSHIP NOT NULL\", \"PATH NOT NULL\""]);
    let r = run(&db, "UNWIND range(1, 50) AS i RETURN collect(DISTINCT randomUUID()) AS ids");
    let ids = match &r.rows[0][0] {
        graphbuff::ResultValue::List(l) => l.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(ids.len(), 50, "UUIDs are distinct");
    for id in ids {
        let s = id.as_str().unwrap().to_string();
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12], "{s}");
        assert!(parts[2].starts_with('4') && "89ab".contains(&parts[3][..1]), "version 4: {s}");
    }
}

#[test]
fn math_functions() {
    let db = Database::in_memory();
    check_exprs(
        &db,
        &[
            ("round(3.14159, 2)", "3.14"),
            ("round(2.675, 2)", "2.68"),
            ("round(-2.675, 2)", "-2.68"),
            ("round(1234.5, -2)", "1200.0"),
            ("round(2.5, 0, 'HALF_EVEN')", "2.0"),
            ("round(3.5, 0, 'HALF_EVEN')", "4.0"),
            ("round(2.5, 0, 'HALF_DOWN')", "2.0"),
            ("round(2.51, 0, 'HALF_DOWN')", "3.0"),
            ("round(1.21, 1, 'UP')", "1.3"),
            ("round(-1.21, 1, 'UP')", "-1.3"),
            ("round(1.29, 1, 'DOWN')", "1.2"),
            ("round(-1.21, 1, 'CEILING')", "-1.2"),
            ("round(-1.21, 1, 'FLOOR')", "-1.3"),
            ("round(0.004, 2, 'UP')", "0.01"),
            ("round(0.004, 2)", "0.0"),
            ("round(7, 3)", "7.0"),
            ("round(null, 2)", "null"),
            ("isNaN(0.0 / 0.0)", "true"),
            ("isNaN(1)", "false"),
            ("atan2(1, 1) = pi() / 4", "true"),
            ("degrees(pi())", "180.0"),
            ("radians(180) = pi()", "true"),
            ("haversin(0)", "0.0"),
            ("round(acos(0.5), 6)", "1.047198"),
            ("round(asin(1), 6)", "1.570796"),
            ("round(atan(1), 6)", "0.785398"),
            ("round(cot(1), 6)", "0.642093"),
        ],
    );
    assert!(db.execute("RETURN round(1.5, 0, 'SIDEWAYS')").is_err());
}

#[test]
fn percentile_aggregates() {
    let db = db_with("UNWIND [10, 20, 30, 40, 50] AS v CREATE (:S {v: v, g: v % 20 = 0})");
    let one = |q: &str| scalar(&db, q).to_string();
    assert_eq!(one("MATCH (s:S) RETURN percentileCont(s.v, 0.5)"), "30.0");
    assert_eq!(one("MATCH (s:S) RETURN percentileCont(s.v, 0.4)"), "26.0");
    assert_eq!(one("MATCH (s:S) RETURN percentileCont(s.v, 1.0)"), "50.0");
    assert_eq!(one("MATCH (s:S) RETURN percentileDisc(s.v, 0.5)"), "30");
    assert_eq!(one("MATCH (s:S) RETURN percentileDisc(s.v, 0.4)"), "20");
    assert_eq!(one("MATCH (s:S) RETURN percentileDisc(s.v, 0.0)"), "10");
    assert_eq!(one("MATCH (s:S) RETURN percentileDisc(toFloat(s.v), 0.9)"), "50.0");
    assert_eq!(one("MATCH (s:S) WHERE s.v > 100 RETURN percentileCont(s.v, 0.5)"), "null");
    let r = run(&db, "MATCH (s:S) RETURN s.g AS g, percentileDisc(s.v, 0.5) AS m ORDER BY g");
    assert_eq!(rows(&r), ["false, 30", "true, 20"]);
    assert!(db.execute("MATCH (s:S) RETURN percentileCont(s.v, 1.5)").is_err());
    assert!(db.execute("MATCH (s:S) RETURN percentileCont(s.v)").is_err());
}

#[test]
fn apoc_style_functions() {
    let db = Database::in_memory();
    check_exprs(
        &db,
        &[
            ("apoc.coll.sum([1, 2, 3.5])", "6.5"),
            ("apoc.coll.avg([1, 2, 3])", "2.0"),
            ("apoc.coll.avg([])", "null"),
            ("apoc.coll.min([3, 1, 2])", "1"),
            ("apoc.coll.max(['a', 'c', 'b'])", "\"c\""),
            ("apoc.coll.toSet([1, 2, 1, 3, 2])", "[1, 2, 3]"),
            ("apoc.coll.sort([3, 1, 2])", "[1, 2, 3]"),
            ("apoc.coll.flatten([[1, 2], [3, [4]]])", "[1, 2, 3, [4]]"),
            ("apoc.coll.flatten([[1, 2], [3, [4]]], true)", "[1, 2, 3, 4]"),
            ("apoc.coll.contains([1, 2], 2)", "true"),
            ("apoc.coll.indexOf([1, 2], 3)", "-1"),
            ("apoc.text.join(['a', 'b', 'c'], '-')", "\"a-b-c\""),
            ("apoc.map.merge({a: 1, b: 2}, {b: 3})", "{a: 1, b: 3}"),
            ("apoc.map.fromPairs([['a', 1], ['b', 2]])", "{a: 1, b: 2}"),
        ],
    );
    assert!(matches!(db.execute("RETURN apoc.coll.shuffle([1])"), Err(Error::Unsupported(_))));
}
