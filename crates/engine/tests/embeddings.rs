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
        vec![ModelInfo { name: "m".into(), dimensions: 2, max_tokens: 8, loaded: false, matryoshka: false, sparse: false, multi_vector: false }]
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
    // A Matryoshka model fits indexes of fewer dimensions, not more.
    let short = Database::in_memory();
    short.set_embedder(Arc::new(HashEmbedder::new([("m", 8)]).matryoshka()));
    short
        .execute("CREATE VECTOR INDEX s FOR (d:Doc) ON (d.e) OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.embedding_model`: 'm'}}")
        .unwrap();
    let e = short
        .execute("CREATE VECTOR INDEX t FOR (d:Doc) ON (d.f) OPTIONS {indexConfig: {`vector.dimensions`: 16, `vector.embedding_model`: 'm'}}")
        .unwrap_err()
        .to_string();
    assert!(e.contains("8 dimensions at most, not 16"), "{e}");
    let r = short.execute("RETURN size(vector.embed('a b c', null, {dimensions: 4})) AS d, size(vector.embed('a b c')) AS full").unwrap();
    assert_eq!((r.rows[0][0].clone(), r.rows[0][1].clone()), (ResultValue::Int(4), ResultValue::Int(8)));
    let r = short
        .execute("CALL vector.embedChunks('one two three four', null, {dimensions: 2}) YIELD vector RETURN size(vector) AS d")
        .unwrap();
    assert_eq!(r.rows[0][0], ResultValue::Int(2));
    let r = short.execute("CALL vector.models() YIELD matryoshka RETURN matryoshka").unwrap();
    assert_eq!(r.rows[0][0], ResultValue::Bool(true));
    let e = db.execute("RETURN vector.embed('a', 'small', {dimensions: 4})").unwrap_err().to_string();
    assert!(e.contains("8 dimensions, not 4"), "{e}");
    // Without models any name is taken.
    let plain = Database::in_memory();
    plain
        .execute("CREATE VECTOR INDEX d FOR (d:Doc) ON (d.e) OPTIONS {indexConfig: {`vector.dimensions`: 8, `vector.embedding_model`: 'elsewhere'}}")
        .unwrap();
}

#[test]
fn sparse_and_multi_vector_indexes_of_embeddings() {
    let (db, _) = hash_db();
    db.execute(
        "UNWIND ['graph databases store edges', 'vectors find neighbours', 'graph search with vectors', 'cooking pasta'] AS text \
         CREATE (d:Doc {text: text}) \
         WITH d, vector.embedSparse(text, 'small') AS w, vector.embedMulti(text, 'small') AS m \
         SET d.lexTokens = keys(w), d.lexWeights = [t IN keys(w) | w[t]], d.colbert = reduce(all = [], v IN m | all + v)",
    )
    .unwrap();
    db.execute("CREATE VECTOR INDEX lex FOR (d:Doc) ON (d.lexTokens, d.lexWeights) OPTIONS {indexConfig: {`vector.type`: 'sparse', `vector.embedding_model`: 'small'}}")
        .unwrap();
    db.execute("CREATE VECTOR INDEX col FOR (d:Doc) ON d.colbert OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 8, `vector.embedding_model`: 'small'}}")
        .unwrap();
    for (index, embed, score) in [
        ("lex", "vector.embedSparse", "vector.sparseScore(q, d.lexTokens, d.lexWeights)"),
        ("col", "vector.embedMulti", "vector.maxSim(q, d.colbert)"),
    ] {
        let search = format!(
            "WITH {embed}($t, 'small', {{task: 'query'}}) AS q \
             CALL db.index.vector.queryNodes('{index}', 2, q) YIELD node, score RETURN node.text AS text, score"
        );
        let scan = format!(
            "WITH {embed}($t, 'small', {{task: 'query'}}) AS q MATCH (d:Doc) WITH d, {score} AS score \
             WHERE score <> 0 RETURN d.text AS text, score ORDER BY score DESC, id(d) LIMIT 2"
        );
        let (a, b) = (with_text(&db, &search, "graph vectors").unwrap(), with_text(&db, &scan, "graph vectors").unwrap());
        assert_eq!(a.rows, b.rows, "{index}");
        assert_eq!(a.len(), 2, "{index}");
    }
    let err = |q: &str| db.execute(q).unwrap_err().to_string();
    let e = err("CREATE VECTOR INDEX x FOR (d:Doc) ON (d.a, d.b) OPTIONS {indexConfig: {`vector.type`: 'sparse', `vector.embedding_model`: 'nope'}}");
    assert!(e.contains("the models are large, small"), "{e}");
    let e = err("CREATE VECTOR INDEX x FOR (d:Doc) ON d.c OPTIONS {indexConfig: {`vector.type`: 'multi', `vector.dimensions`: 4, `vector.embedding_model`: 'small'}}");
    assert!(e.contains("8 dimensions, not 4"), "{e}");
}

fn with_text(db: &Database<MemoryGraph>, q: &str, text: &str) -> Result<QueryResult> {
    let params = [("t".to_string(), graphbuff::Value::String(text.into()))].into_iter().collect();
    db.execute_with_params(q, &params)
}

fn strings(r: &QueryResult, col: &str) -> Vec<String> {
    (0..r.len())
        .map(|i| match r.get(i, col) {
            Some(ResultValue::String(s)) => s.clone(),
            o => panic!("{col}: {o:?}"),
        })
        .collect()
}

#[test]
fn chunks_fit_the_model_and_point_into_the_text() {
    let e = Arc::new(HashEmbedder::new([("m", 8)]).with_max_tokens(10));
    let db = db_with(e.clone());
    let text = "Příliš žluťoučký kůň úpěl ďábelské ódy. ".repeat(6) + "\n\nKonec.";
    let r = with_text(
        &db,
        "CALL vector.chunk($t) YIELD index, text, start, length, tokens \
         RETURN index, text, tokens, substring($t, start, length) = text AS same",
        &text,
    )
    .unwrap();
    assert!(r.len() >= 4, "{}", r.len());
    for i in 0..r.len() {
        assert_eq!(r.get(i, "index"), Some(&ResultValue::Int(i as i64)));
        assert_eq!(r.get(i, "same"), Some(&ResultValue::Bool(true)), "row {i}: start and length count characters");
        let Some(ResultValue::Int(t)) = r.get(i, "tokens") else { panic!() };
        assert!((1..=10).contains(t));
    }
    let chunks = strings(&r, "text");
    assert!(chunks.iter().take(chunks.len() - 1).all(|c| c.ends_with("ódy.")), "cut at sentences: {chunks:?}");
    assert!(chunks.last().unwrap().ends_with("ódy. 

Konec."), "what fits stays together: {chunks:?}");
    // A query's prefix takes a token: chunks of 9.
    let r = with_text(&db, "CALL vector.chunk($t, null, {task: 'query', size: 9}) YIELD tokens RETURN max(tokens) AS m", &text).unwrap();
    assert!(matches!(one(&r, "m"), ResultValue::Int(n) if n <= 9));
    let e2 = with_text(&db, "CALL vector.chunk($t, null, {task: 'query', size: 10}) YIELD text RETURN text", &text).unwrap_err();
    assert!(e2.to_string().contains("`size` 10 is more than the 9 tokens"), "{e2}");
}

#[test]
fn embed_chunks_embeds_each_chunk() {
    let e = Arc::new(HashEmbedder::new([("m", 8), ("other", 16)]).with_max_tokens(6));
    let db = db_with(e.clone());
    let text = "graph nodes edges. vector index search. text model token layer query.";
    let r = with_text(
        &db,
        "CALL vector.embedChunks($t, 'other', {overlap: 2, task: 'query'}) YIELD index, text, vector RETURN *",
        text,
    )
    .unwrap();
    assert!(r.len() >= 3);
    let query = EmbedOptions { task: EmbedTask::Query, truncate: true, dimensions: None };
    for (i, chunk) in strings(&r, "text").iter().enumerate() {
        let v = floats(r.get(i, "vector").unwrap());
        assert_eq!(v.len(), 16);
        assert_eq!(v, expected(&e, Some("other"), chunk, &query), "{chunk}");
    }
    let calls = e.calls();
    with_text(&db, "CALL vector.embedChunks($t) YIELD vector RETURN vector", text).unwrap();
    assert_eq!(e.calls(), calls + 1, "one embedding call for all chunks");
}

#[test]
fn chunk_arguments_are_checked() {
    let (db, _) = hash_db();
    for q in ["CALL vector.chunk(null) YIELD text RETURN text", "CALL vector.embedChunks('') YIELD text RETURN text"] {
        assert_eq!(db.execute(q).unwrap().len(), 0, "{q}");
    }
    for (q, err) in [
        ("CALL vector.chunk(5) YIELD text RETURN text", "expects STRING"),
        ("CALL vector.chunk('a b', 'nope') YIELD text RETURN text", "unknown embedding model"),
        ("CALL vector.chunk('a b', null, {overlap: 64}) YIELD text RETURN text", "`overlap` (64) must be less than the size (64)"),
        ("CALL vector.chunk('a b', null, {size: 65}) YIELD text RETURN text", "more than the 64 tokens"),
        ("CALL vector.chunk('a b', null, {sizes: 5}) YIELD text RETURN text", "unknown option `sizes`"),
    ] {
        let e = db.execute(q).unwrap_err().to_string();
        assert!(e.contains(err), "{q}: {e}");
    }
    // An embedder that cannot tell its tokens.
    struct Plain;
    impl Embedder for Plain {
        fn models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        fn embed(&self, _: Option<&str>, t: &[&str], _: &EmbedOptions, _: &dyn Fn() -> Result<()>) -> Result<Vec<Vec<f32>>> {
            Ok(vec![vec![1.0]; t.len()])
        }
    }
    let db = db_with(Arc::new(Plain));
    assert!(matches!(db.execute("CALL vector.chunk('a') YIELD text RETURN text"), Err(Error::Unsupported(_))));
}

#[test]
fn lexical_weights_and_per_token_vectors() {
    let (db, _) = hash_db();
    let one = |q: &str| db.execute(q).unwrap().rows[0][0].clone();
    // HashEmbedder: weight 1 per distinct word; a vector per word.
    assert_eq!(one("RETURN size(keys(vector.embedSparse('a b a c'))) AS n"), ResultValue::Int(3));
    assert_eq!(one("RETURN vector.embedSparse('a b a c').a AS w"), ResultValue::Float(1.0));
    assert_eq!(one("RETURN vector.embedSparse(null) AS w"), ResultValue::Null);
    assert_eq!(one("RETURN size(vector.embedMulti('a b c', 'large')) AS n"), ResultValue::Int(3));
    assert_eq!(one("RETURN size(vector.embedMulti('a b c', 'large')[0]) AS n"), ResultValue::Int(32));
    // Scores.
    assert_eq!(one("RETURN vector.sparseScore({a: 2, b: 0.5}, {b: 4.0, c: 9}) AS s"), ResultValue::Float(2.0));
    assert_eq!(one("RETURN vector.sparseScore({a: 1}, {}) AS s"), ResultValue::Float(0.0));
    assert_eq!(one("RETURN vector.sparseScore(null, {a: 1}) AS s"), ResultValue::Null);
    // q1 best matches d1 (1), q2 best matches d2 (0.5): mean 0.75.
    assert_eq!(one("RETURN vector.maxSim([[1, 0], [0, 1]], [[1, 0], [0, 0.5]]) AS s"), ResultValue::Float(0.75));
    assert_eq!(one("RETURN vector.maxSim(vector.embedMulti('x y'), vector.embedMulti('y x x')) AS s"), ResultValue::Float(1.0));
    assert_eq!(one("RETURN vector.maxSim(null, [[1]]) AS s"), ResultValue::Null);
    let err = |q: &str| db.execute(q).unwrap_err().to_string();
    assert!(err("RETURN vector.maxSim([[1, 0]], [[1]])").contains("different dimensions"));
    assert!(err("RETURN vector.maxSim([], [[1]])").contains("at least one"));
    assert!(err("RETURN vector.sparseScore({a: 'x'}, {})").contains("finite numbers"));
    assert!(err("RETURN vector.sparseScore([1], {})").contains("MAP"));
    assert!(err("RETURN vector.embedSparse('a', null, {dimensions: 4})").contains("single vectors"));
    // embedBatch with output.
    let r = db
        .execute("CALL vector.embedBatch(['a b', null], null, {output: 'sparse'}) YIELD index, vector RETURN index, vector")
        .unwrap();
    assert!(matches!(&r.rows[0][1], ResultValue::Map(m) if m.len() == 2));
    assert_eq!(r.rows[1][1], ResultValue::Null);
    assert!(err("CALL vector.embedBatch(['a'], null, {output: 'colbert'}) YIELD vector RETURN vector").contains("'dense', 'sparse' or 'multi'"));
    let r = db.execute("CALL vector.models() YIELD name, sparse, multiVector RETURN sparse, multiVector LIMIT 1").unwrap();
    assert_eq!(r.rows[0], vec![ResultValue::Bool(true), ResultValue::Bool(true)]);

    // Stored as properties (no maps, no lists of lists): tokens and weights, one flat list.
    db.execute(
        "WITH 'graph nodes edges' AS text \
         WITH text, vector.embedSparse(text) AS w, vector.embedMulti(text) AS m \
         CREATE (:Doc {text: text, lexTokens: keys(w), lexWeights: [t IN keys(w) | w[t]], \
                       colbert: reduce(all = [], v IN m | all + v)})",
    )
    .unwrap();
    let r = db
        .execute(
            "MATCH (d:Doc) WITH d, vector.embedSparse('graph edges') AS q, vector.embedMulti('graph edges') AS qm \
             RETURN vector.sparseScore(q, d.lexTokens, d.lexWeights) AS lexical, vector.maxSim(qm, d.colbert) AS colbert, \
                    vector.maxSim(qm, vector.embedMulti(d.text)) AS nested, size(d.colbert) AS floats",
        )
        .unwrap();
    assert_eq!(r.get(0, "lexical"), Some(&ResultValue::Float(2.0)));
    assert_eq!(r.get(0, "colbert"), Some(&ResultValue::Float(1.0)));
    assert_eq!(r.get(0, "colbert"), r.get(0, "nested"));
    assert_eq!(r.get(0, "floats"), Some(&ResultValue::Int(24)), "3 words of 8 dimensions");
    assert!(err("RETURN vector.sparseScore({a: 1}, ['a', 'b'], [1.0])").contains("2 tokens but 1 weights"));
    assert!(err("RETURN vector.maxSim([[1, 0]], [1, 0, 1])").contains("not vectors of the query's 2 dimensions"));
}
