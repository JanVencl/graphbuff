//! Embeddings, end to end with the model-free [`HashEmbedder`]: `vector.embed`,
//! `vector.embedBatch`, `vector.models`, the error without an embedder, and embedded
//! vectors in a vector index.

use std::sync::Arc;

use graphbuff::{
    CancelHandle, DEFAULT_DATABASE, Database, DatabaseManager, EmbedOptions, EmbedTask, Embedder, Error, HashEmbedder, MemoryGraph, ModelInfo,
    QueryOptions, QueryResult, ResultValue, Result,
};

fn db_with(embedder: Arc<dyn Embedder>) -> Database<MemoryGraph> {
    let db = Database::in_memory();
    db.set_embedder(embedder);
    db
}

fn hash_db() -> (Database<MemoryGraph>, Arc<HashEmbedder>) {
    let e = Arc::new(HashEmbedder::new([("small", 8), ("large", 32)]));
    (db_with(e.clone()), e)
}

fn floats(v: &ResultValue) -> Vec<f64> {
    match v {
        ResultValue::List(l) => l
            .iter()
            .map(|x| match x {
                ResultValue::Float(f) => *f,
                o => panic!("not a float: {o:?}"),
            })
            .collect(),
        o => panic!("not a list: {o:?}"),
    }
}

fn one(r: &QueryResult, col: &str) -> ResultValue {
    r.get(0, col).cloned().unwrap_or_else(|| panic!("no {col}"))
}

fn expected(e: &HashEmbedder, model: Option<&str>, text: &str, options: &EmbedOptions) -> Vec<f64> {
    e.embed(model, &[text], options, &|| Ok(())).unwrap()[0].iter().map(|x| *x as f64).collect()
}

#[test]
fn without_an_embedder_everything_fails_clearly() {
    let db = Database::in_memory();
    for q in [
        "RETURN vector.embed('x') AS v",
        "CALL vector.embedBatch(['x']) YIELD vector RETURN vector",
        "CALL vector.models() YIELD name RETURN name",
    ] {
        match db.execute(q) {
            Err(Error::Unsupported(m)) => assert!(m.contains("embedding is not available"), "{q}: {m}"),
            other => panic!("{q}: {other:?}"),
        }
    }
    // A null text needs no model.
    assert_eq!(one(&db.execute("RETURN vector.embed(null) AS v").unwrap(), "v"), ResultValue::Null);
}

#[test]
fn embed_uses_the_default_or_named_model() {
    let (db, e) = hash_db();
    let r = db.execute("RETURN vector.embed('Graph databases') AS d, vector.embed('Graph databases', 'large') AS l").unwrap();
    assert_eq!(floats(&one(&r, "d")), expected(&e, Some("small"), "Graph databases", &EmbedOptions::default()));
    assert_eq!(floats(&one(&r, "l")).len(), 32);
    // `null` model: the default.
    let r = db.execute("RETURN size(vector.embed('x', null)) AS n").unwrap();
    assert_eq!(r.get(0, "n"), Some(&ResultValue::Int(8)));
    // Upper-case name of the function works like any other.
    let r = db.execute("RETURN size(VECTOR.EMBED('x')) AS n").unwrap();
    assert_eq!(r.get(0, "n"), Some(&ResultValue::Int(8)));
}

#[test]
fn embed_arguments_are_checked() {
    let (db, _) = hash_db();
    assert!(matches!(db.execute("RETURN vector.embed() AS v"), Err(Error::Semantic(m)) if m.contains("expects 1 to 3")));
    assert!(matches!(db.execute("RETURN vector.embed('a', 'small', {}, 1) AS v"), Err(Error::Semantic(m)) if m.contains("expects 1 to 3")));
    assert!(matches!(db.execute("RETURN vector.embed(42) AS v"), Err(Error::Type(_))));
    assert!(matches!(db.execute("RETURN vector.embed('a', 7) AS v"), Err(Error::Type(_))));
    assert!(matches!(db.execute("RETURN vector.embed('a', null, 'x') AS v"), Err(Error::Type(_))));
    assert!(matches!(db.execute("RETURN vector.embed('a', null, {colour: 1}) AS v"), Err(Error::Semantic(m)) if m.contains("unknown option")));
    assert!(matches!(db.execute("RETURN vector.embed('a', null, {task: 'x'}) AS v"), Err(Error::Semantic(_))));
    match db.execute("RETURN vector.embed('a', 'nope') AS v") {
        Err(Error::Semantic(m)) => assert!(m.contains("unknown embedding model 'nope'"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(one(&db.execute("RETURN vector.embed(null, 'nope') AS v").unwrap(), "v"), ResultValue::Null);
}

#[test]
fn options_reach_the_embedder() {
    let e = Arc::new(HashEmbedder::new([("m", 16)]).with_max_tokens(3));
    let db = db_with(e.clone());
    let r = db.execute("RETURN vector.embed('a b', null, {task: 'query'}) AS q, vector.embed('a b') AS d").unwrap();
    let query = EmbedOptions { task: EmbedTask::Query, ..Default::default() };
    assert_eq!(floats(&one(&r, "q")), expected(&e, None, "a b", &query));
    assert_ne!(one(&r, "q"), one(&r, "d"));
    // Too long: cut by default, an error with truncate: false.
    assert!(db.execute("RETURN vector.embed('a b c d e') AS v").is_ok());
    assert!(matches!(db.execute("RETURN vector.embed('a b c d e', null, {truncate: false}) AS v"), Err(Error::Semantic(m)) if m.contains("tokens")));
}

#[test]
fn embed_batch_keeps_order_and_nulls_in_one_call() {
    let (db, e) = hash_db();
    let before = e.calls();
    let r = db
        .execute("CALL vector.embedBatch(['one', null, 'two', 'three'], 'large') YIELD index, text, vector RETURN index, text, vector")
        .unwrap();
    assert_eq!(e.calls() - before, 1, "one embed call for the whole batch");
    assert_eq!(r.len(), 4);
    for (i, text) in ["one", "", "two", "three"].iter().enumerate() {
        assert_eq!(r.get(i, "index"), Some(&ResultValue::Int(i as i64)));
        if i == 1 {
            assert_eq!(r.get(i, "text"), Some(&ResultValue::Null));
            assert_eq!(r.get(i, "vector"), Some(&ResultValue::Null));
        } else {
            assert_eq!(r.get(i, "text"), Some(&ResultValue::String(text.to_string())));
            assert_eq!(floats(r.get(i, "vector").unwrap()), expected(&e, Some("large"), text, &EmbedOptions::default()));
        }
    }
    // Same vectors as the scalar function.
    let s = db.execute("RETURN vector.embed('two', 'large') AS v").unwrap();
    assert_eq!(one(&s, "v"), r.get(2, "vector").unwrap().clone());

    let empty = db.execute("CALL vector.embedBatch([]) YIELD vector RETURN count(*) AS n").unwrap();
    assert_eq!(empty.get(0, "n"), Some(&ResultValue::Int(0)));
    assert!(matches!(db.execute("CALL vector.embedBatch(['a', 1]) YIELD vector RETURN vector"), Err(Error::Type(_))));
    let r = db.execute("CALL vector.embedBatch('a') YIELD vector RETURN vector");
    assert!(matches!(&r, Err(Error::Semantic(m)) if m.contains("expects LIST")), "{r:?}");
    assert!(matches!(db.execute("CALL vector.embedBatch(['a'], 'nope') YIELD vector RETURN vector"), Err(Error::Semantic(_))));
}

#[test]
fn models_are_listed_by_name() {
    let (db, _) = hash_db();
    let r = db.execute("CALL vector.models() YIELD name, dimensions, maxTokens, loaded RETURN *").unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r.get(0, "name"), Some(&ResultValue::String("large".into())));
    assert_eq!(r.get(0, "dimensions"), Some(&ResultValue::Int(32)));
    assert_eq!(r.get(1, "name"), Some(&ResultValue::String("small".into())));
    assert_eq!(r.get(1, "maxTokens"), Some(&ResultValue::Int(64)));
    assert_eq!(r.get(1, "loaded"), Some(&ResultValue::Bool(true)));
    let f = db.execute("CALL dbms.functions() YIELD name WHERE name = 'vector.embed' RETURN name").unwrap();
    assert_eq!(f.len(), 1);
}

#[test]
fn embedded_vectors_go_into_a_vector_index() {
    let (db, _) = hash_db();
    db.execute(
        "UNWIND ['graph databases store nodes', 'cats sleep all day', 'vector search over graph nodes'] AS t \
         CREATE (:Doc {text: t})",
    )
    .unwrap();
    db.execute("MATCH (d:Doc) SET d.embedding = vector.embed(d.text, 'large')").unwrap();
    db.execute("CREATE VECTOR INDEX docs FOR (d:Doc) ON d.embedding OPTIONS {indexConfig: {`vector.dimensions`: 32}}")
        .unwrap();
    db.execute("CALL db.awaitIndexes()").unwrap();
    let r = db
        .execute(
            "CALL db.index.vector.queryNodes('docs', 1, vector.embed('cats sleep all day', 'large')) \
             YIELD node, score RETURN node.text AS text, score",
        )
        .unwrap();
    assert_eq!(r.get(0, "text"), Some(&ResultValue::String("cats sleep all day".into())));
    assert!(matches!(r.get(0, "score"), Some(ResultValue::Float(f)) if (f - 1.0).abs() < 1e-6));

    // The batch path in a writing query.
    db.execute("CREATE (:Doc {text: 'late arrival'})").unwrap();
    db.execute(
        "MATCH (d:Doc) WHERE d.embedding IS NULL WITH collect(d) AS docs \
         CALL vector.embedBatch([d IN docs | d.text], 'large') YIELD index, vector \
         WITH docs[index] AS d, vector SET d.embedding = vector",
    )
    .unwrap();
    let n = db.execute("MATCH (d:Doc) WHERE size(d.embedding) = 32 RETURN count(d) AS n").unwrap();
    assert_eq!(n.get(0, "n"), Some(&ResultValue::Int(4)));
}

#[test]
fn works_inside_a_transaction() {
    let (db, _) = hash_db();
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:Doc {v: vector.embed('in a transaction')})").unwrap();
    let r = tx.execute("CALL vector.embedBatch(['a', 'b']) YIELD vector RETURN count(vector) AS n").unwrap();
    assert_eq!(r.get(0, "n"), Some(&ResultValue::Int(2)));
    tx.commit().unwrap();
    let r = db.execute("MATCH (d:Doc) RETURN size(d.v) AS n").unwrap();
    assert_eq!(r.get(0, "n"), Some(&ResultValue::Int(8)));
}

#[test]
fn the_manager_sets_every_database_now_and_later() {
    let mgr = DatabaseManager::in_memory();
    let mut s = mgr.session();
    s.execute("CREATE DATABASE before").unwrap();
    mgr.set_embedder(Arc::new(HashEmbedder::new([("m", 4)])));
    s.execute("CREATE DATABASE after").unwrap();
    for db in [DEFAULT_DATABASE, "before", "after"] {
        s.execute(&format!("USE {db}")).unwrap();
        let r = s.execute("RETURN size(vector.embed('x')) AS n").unwrap();
        assert_eq!(r.get(0, "n"), Some(&ResultValue::Int(4)), "{db}");
    }
}

/// An embedder that cancels its own query, then asks whether to stop: the interrupt it
/// was given must say yes.
struct SelfCancelling(CancelHandle);

impl Embedder for SelfCancelling {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo { name: "m".into(), dimensions: 2, max_tokens: 8, loaded: false }]
    }

    fn embed(&self, _: Option<&str>, texts: &[&str], _: &EmbedOptions, interrupt: &dyn Fn() -> Result<()>) -> Result<Vec<Vec<f32>>> {
        self.0.cancel();
        interrupt()?;
        Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
    }
}

#[test]
fn the_embedder_sees_the_query_interrupt() {
    for q in ["RETURN vector.embed('x') AS v", "CALL vector.embedBatch(['x']) YIELD vector RETURN vector"] {
        // Without limits there is nothing to interrupt.
        let db = db_with(Arc::new(SelfCancelling(CancelHandle::new())));
        assert!(db.execute(q).is_ok(), "{q}");
        // Not cancelled until the embedder runs.
        let cancel = CancelHandle::new();
        let db = db_with(Arc::new(SelfCancelling(cancel.clone())));
        let options = QueryOptions::default().with_cancel(cancel);
        let r = db.execute_with_options(q, &Default::default(), &options);
        assert!(matches!(r, Err(Error::Cancelled(_))), "{q}: {r:?}");
    }
}

#[test]
fn a_wrong_vector_count_from_the_embedder_is_an_error() {
    struct Short;
    impl Embedder for Short {
        fn models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        fn embed(&self, _: Option<&str>, _: &[&str], _: &EmbedOptions, _: &dyn Fn() -> Result<()>) -> Result<Vec<Vec<f32>>> {
            Ok(Vec::new())
        }
    }
    let db = db_with(Arc::new(Short));
    assert!(matches!(db.execute("RETURN vector.embed('x') AS v"), Err(Error::Runtime(_))));
    let r = db.execute("CALL vector.models() YIELD name RETURN name").unwrap();
    assert_eq!(r.len(), 0);
}

#[test]
fn stored_vectors_survive_a_restart_without_an_embedder() {
    // The log and snapshots keep the vectors, not the calls: recovery runs no model.
    let dir = std::env::temp_dir().join(format!("graphbuff-embed-durable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = graphbuff::Database::open(graphbuff::StorageMode::hybrid(&dir)).unwrap();
        db.set_embedder(Arc::new(HashEmbedder::new([("m", 8)])));
        db.execute("CREATE (:Doc {v: vector.embed('kept')})").unwrap();
    }
    let db = graphbuff::Database::open(graphbuff::StorageMode::hybrid(&dir)).unwrap();
    let r = db.execute("MATCH (d:Doc) RETURN d.v AS v").unwrap();
    let expected = HashEmbedder::new([("m", 8)]).embed(None, &["kept"], &EmbedOptions::default(), &|| Ok(())).unwrap();
    assert_eq!(floats(&one(&r, "v")), expected[0].iter().map(|x| *x as f64).collect::<Vec<_>>());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_vector_index_records_its_model() {
    let (db, _) = hash_db();
    db.execute(
        "CREATE VECTOR INDEX docs FOR (d:Doc) ON (d.e) OPTIONS {indexConfig: {`vector.dimensions`: 8, `vector.embedding_model`: 'small'}}",
    )
    .unwrap();
    let r = db.execute("CALL db.indexes() YIELD name, options WITH name, options WHERE name = 'docs' RETURN options.indexConfig.`vector.embedding_model` AS m").unwrap();
    assert_eq!(r.rows[0][0], ResultValue::String("small".into()));
    let err = |q: &str| db.execute(q).unwrap_err().to_string();
    let e = err("CREATE VECTOR INDEX a FOR (d:Doc) ON (d.a) OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.embedding_model`: 'small'}}");
    assert!(e.contains("8 dimensions, not 4"), "{e}");
    let e = err("CREATE VECTOR INDEX b FOR (d:Doc) ON (d.b) OPTIONS {indexConfig: {`vector.dimensions`: 8, `vector.embedding_model`: 'nope'}}");
    assert!(e.contains("the models are large, small"), "{e}");
    let e = err("CREATE VECTOR INDEX c FOR (d:Doc) ON (d.c) OPTIONS {indexConfig: {`vector.dimensions`: 8, `vector.embedding_model`: 3}}");
    assert!(e.contains("model name"), "{e}");
    // Without models any name is taken.
    let plain = Database::in_memory();
    plain
        .execute("CREATE VECTOR INDEX d FOR (d:Doc) ON (d.e) OPTIONS {indexConfig: {`vector.dimensions`: 8, `vector.embedding_model`: 'elsewhere'}}")
        .unwrap();
}
