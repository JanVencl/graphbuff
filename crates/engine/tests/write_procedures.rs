//! Write procedures: `db.create.setNodeVectorProperty` / `setRelationshipVectorProperty`,
//! and procedures an application registers with `ProcedureSignature::writes`.

use graphbuff::{Database, Error, ProcedureSignature, ProcedureType, QueryResult, Value};

fn run(db: &Database, q: &str) -> QueryResult {
    db.execute(q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
}

fn rows(db: &Database, q: &str) -> Vec<String> {
    run(db, q)
        .rows
        .iter()
        .map(|r| r.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect()
}

fn err(db: &Database, q: &str) -> String {
    match db.execute(q) {
        Ok(_) => panic!("`{q}` should fail"),
        Err(e) => e.to_string(),
    }
}

/// `test.tag(node, label, mark)`: adds `label`, sets `mark` to the number of nodes with
/// the label before the call (so each call shows what the earlier ones did), and returns it.
fn with_tag(db: &Database) {
    db.register_procedure(
        ProcedureSignature::new("test.tag", ["node", "label"], ["before"])
            .input_types([ProcedureType::Node, ProcedureType::String])
            .writes(),
        |ctx, args| {
            let (Value::Node(id), Value::String(label)) = (&args[0], &args[1]) else {
                return Ok(Vec::new());
            };
            let before = ctx.store().label_count(label) as i64;
            ctx.add_label(*id, label)?;
            ctx.set_property(&args[0], "mark", Value::Int(before))?;
            Ok(vec![vec![Value::Int(before)]])
        },
    )
    .unwrap();
}

#[test]
fn set_vector_properties() {
    let db = Database::in_memory();
    run(&db, "CREATE VECTOR INDEX docs FOR (d:Doc) ON (d.v) OPTIONS {indexConfig: {`vector.dimensions`: 2}}");
    run(&db, "CREATE (:Doc {k: 1}), (:Doc {k: 2})-[:SEE {k: 3}]->(:Doc {k: 4})");
    let r = run(&db, "MATCH (d:Doc) CALL db.create.setNodeVectorProperty(d, 'v', [d.k, 1])");
    assert_eq!(r.stats.properties_set, 3);
    assert!(r.columns.is_empty());
    assert_eq!(rows(&db, "MATCH (d:Doc {k: 2}) RETURN d.v"), ["[2.0, 1.0]"]);
    assert_eq!(
        rows(&db, "CALL db.index.vector.queryNodes('docs', 1, [4, 1]) YIELD node RETURN node.k"),
        ["4"]
    );
    run(&db, "MATCH ()-[r:SEE]->() CALL db.create.setRelationshipVectorProperty(r, 'v', [0.5, -1e3]) RETURN r");
    assert_eq!(rows(&db, "MATCH ()-[r:SEE]->() RETURN r.v"), ["[0.5, -1000.0]"]);
    // Standalone, with parameters; null vector removes, null entity does nothing.
    run(&db, "MATCH (d:Doc {k: 1}) CALL db.create.setNodeVectorProperty(d, 'v', null)");
    assert_eq!(rows(&db, "MATCH (d:Doc {k: 1}) RETURN d.v"), ["null"]);
    run(&db, "OPTIONAL MATCH (x:Nothing) CALL db.create.setNodeVectorProperty(x, 'v', [1]) RETURN x");
    for bad in [
        "MATCH (d:Doc {k: 2}) CALL db.create.setNodeVectorProperty(d, 'v', [1, 'a'])",
        "MATCH (d:Doc {k: 2}) CALL db.create.setNodeVectorProperty(d, 'v', [])",
        "MATCH (d:Doc {k: 2}) CALL db.create.setNodeVectorProperty(d, 'v', [1, 0.0 / 0.0])",
        "MATCH (d:Doc {k: 2}) CALL db.create.setNodeVectorProperty(d, 'v', 'abc')",
        "MATCH (d:Doc {k: 2}) CALL db.create.setNodeVectorProperty(d, null, [1])",
        "MATCH ()-[r:SEE]->() CALL db.create.setNodeVectorProperty(r, 'v', [1])",
        "MATCH (d:Doc {k: 2}) CALL db.create.setNodeVectorProperty(d, 'v', [1]) YIELD x RETURN x",
    ] {
        err(&db, bad);
    }
    // A failed call changes nothing.
    assert_eq!(rows(&db, "MATCH (d:Doc {k: 2}) RETURN d.v"), ["[2.0, 1.0]"]);
}

#[test]
fn listed_with_their_mode() {
    let db = Database::in_memory();
    with_tag(&db);
    let q = "SHOW PROCEDURES YIELD name, mode WHERE name IN ['db.labels', 'db.create.setNodeVectorProperty', 'test.tag'] RETURN name, mode";
    let listed = match db.execute(q) {
        Ok(_) => rows(&db, q),
        // Without YIELD on SHOW: the dbms.procedures() form.
        Err(_) => rows(
            &db,
            "CALL dbms.procedures() YIELD name, mode WHERE name IN ['db.labels', 'db.create.setNodeVectorProperty', 'test.tag'] RETURN name, mode",
        ),
    };
    assert_eq!(
        listed,
        ["\"db.create.setNodeVectorProperty\", \"WRITE\"", "\"db.labels\", \"READ\"", "\"test.tag\", \"WRITE\""]
    );
    let all = run(&db, "SHOW PROCEDURES");
    assert_eq!(all.columns, ["name", "signature", "description", "mode"]);
}

#[test]
fn registered_write_procedures() {
    let db = Database::in_memory();
    with_tag(&db);
    run(&db, "UNWIND range(1, 3) AS k CREATE (:P {k: k})");
    let r = run(&db, "MATCH (p:P) WITH p ORDER BY p.k CALL test.tag(p, 'Done') YIELD before RETURN p.k, before");
    // Each call sees the changes of the calls before it.
    let got: Vec<String> = r.rows.iter().map(|r| format!("{}, {}", r[0], r[1])).collect();
    assert_eq!(got, ["1, 0", "2, 1", "3, 2"]);
    assert_eq!((r.stats.labels_added, r.stats.properties_set), (3, 3));
    assert_eq!(rows(&db, "MATCH (p:Done) RETURN p.k, p.mark ORDER BY p.k"), ["1, 0", "2, 1", "3, 2"]);
    // A later failure rolls the whole query back.
    let e = err(&db, "MATCH (p:P) CALL test.tag(p, 'Again') YIELD before RETURN 1 / (before - 2) AS x");
    assert!(e.contains("zero") || e.contains("/"), "{e}");
    assert!(rows(&db, "MATCH (p:Again) RETURN p").is_empty());
    // EXPLAIN plans it without running it.
    let plan = db.explain("MATCH (p:P) CALL test.tag(p, 'X') YIELD before RETURN before").unwrap();
    assert!(plan.contains("ProcedureCall test.tag() YIELD before"), "{plan}");
    assert!(rows(&db, "MATCH (p:X) RETURN p").is_empty());
}

#[test]
fn read_procedures_cannot_write() {
    let db = Database::in_memory();
    db.register_procedure(ProcedureSignature::new("test.sneak", ["node"], ["ok"]), |ctx, args| {
        ctx.set_property(&args[0], "x", Value::Int(1))?;
        Ok(vec![vec![Value::Bool(true)]])
    })
    .unwrap();
    run(&db, "CREATE (:P)");
    let e = err(&db, "MATCH (p:P) CALL test.sneak(p) YIELD ok RETURN ok");
    assert!(e.contains("read procedure cannot change the graph"), "{e}");
    assert_eq!(rows(&db, "MATCH (p:P) RETURN p.x"), ["null"]);
    // Nor can write procedures inside an expression subquery.
    with_tag(&db);
    let e = err(&db, "MATCH (p:P) RETURN COUNT { CALL test.tag(p, 'Y') YIELD before RETURN before } AS n");
    assert!(!e.is_empty());
    assert!(rows(&db, "MATCH (p:Y) RETURN p").is_empty());
}

#[test]
fn writes_query_and_access() {
    let db = Database::in_memory();
    with_tag(&db);
    let parse = |q: &str| graphbuff::cypher::parse(q).unwrap();
    assert!(db.writes(&parse("MATCH (p) CALL test.tag(p, 'A') YIELD before RETURN before")));
    assert!(db.writes(&parse("MATCH (p) CALL { WITH p CALL db.create.setNodeVectorProperty(p, 'v', [1]) }")));
    assert!(!db.writes(&parse("CALL db.labels()")));
    assert!(!db.writes(&parse("MATCH (p) RETURN p")));
    run(&db, "CREATE (:P)");
    db.set_read_only(true);
    let e = db.execute("MATCH (p:P) CALL db.create.setNodeVectorProperty(p, 'v', [1])").unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "{e}");
    db.set_read_only(false);
    // CALL { … } IN TRANSACTIONS: inside, yes; around it, no.
    run(&db, "MATCH (p:P) CALL { WITH p CALL db.create.setNodeVectorProperty(p, 'v', [7]) } IN TRANSACTIONS");
    assert_eq!(rows(&db, "MATCH (p:P) RETURN p.v"), ["[7.0]"]);
    let e = err(
        &db,
        "MATCH (p:P) CALL db.create.setNodeVectorProperty(p, 'v', [1]) CALL { WITH p SET p.y = 1 } IN TRANSACTIONS",
    );
    assert!(e.contains("outside CALL"), "{e}");
}

#[test]
fn inside_transactions() {
    let db = Database::in_memory();
    run(&db, "CREATE (:P {k: 1})");
    let mut tx = db.begin().unwrap();
    tx.execute("MATCH (p:P) CALL db.create.setNodeVectorProperty(p, 'v', [1, 2])").unwrap();
    assert_eq!(tx.execute("MATCH (p:P) RETURN p.v").unwrap().rows[0][0].to_string(), "[1.0, 2.0]");
    assert_eq!(rows(&db, "MATCH (p:P) RETURN p.v"), ["null"]);
    tx.commit().unwrap();
    assert_eq!(rows(&db, "MATCH (p:P) RETURN p.v"), ["[1.0, 2.0]"]);
}
