//! Vectors against Hugging Face `transformers`: the tiny random BERT fixture always (its
//! reference comes from scripts/make_fixture.py), a real model when one is given:
//!
//! ```text
//! python scripts/reference.py path/to/model/minilm      # writes reference.json there
//! GRAPHBUFF_EMBED_TEST_MODEL=path/to/model/minilm cargo test -p graphbuff-embed --release -- --ignored
//! ```

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Reference, TempDir, assert_close, fixture};
use graphbuff::{Database, EmbedOptions, EmbedTask, Embedder, Error, ResultValue};
use graphbuff_embed::{EmbedderConfig, LocalEmbedder, ModelSpec, Registry};

fn embedder_for(dir: &Path, name: &str, batch_size: usize) -> LocalEmbedder {
    let spec = ModelSpec::read(name, dir).unwrap();
    LocalEmbedder::new(Registry::from_specs([spec]), EmbedderConfig { batch_size, threads: 2, ..Default::default() }).unwrap()
}

fn no_interrupt() -> graphbuff::Result<()> {
    Ok(())
}

/// Token ids and vectors of every reference text, one call for all and one per text.
fn check_model(dir: &Path, name: &str, tol: f64) {
    let reference = Reference::read(&dir.join("reference.json"));
    let texts = reference.texts();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let want = reference.vectors(if reference.has("mean") { "mean" } else { "vectors" });
    for batch_size in [1, 3, 32] {
        let e = embedder_for(dir, name, batch_size);
        let opts = EmbedOptions::default();
        for (i, ids) in reference.ids("ids").iter().enumerate() {
            assert_eq!(&e.token_ids(None, &texts[i], &opts).unwrap(), ids, "token ids of {:?}", texts[i]);
        }
        let got = e.embed(None, &refs, &opts, &no_interrupt).unwrap();
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_close(&format!("batch {batch_size}, text {i} {:?}", texts[i]), g, w, tol);
        }
        let single = e.embed(None, &refs[..1], &opts, &no_interrupt).unwrap();
        assert_close("alone", &single[0], &want[0], tol);
    }
}

#[test]
fn tiny_bert_matches_transformers() {
    check_model(&fixture(), "tiny-bert", 1e-5);
}

#[test]
fn query_prefix_and_truncation() {
    let reference = Reference::read(&fixture().join("reference.json"));
    let texts = reference.texts();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let e = embedder_for(&fixture(), "tiny-bert", 32);
    let query = EmbedOptions { task: EmbedTask::Query, ..Default::default() };
    for (i, ids) in reference.ids("query_ids").iter().enumerate() {
        assert_eq!(&e.token_ids(None, &texts[i], &query).unwrap(), ids);
    }
    let got = e.embed(None, &refs, &query, &no_interrupt).unwrap();
    for (i, (g, w)) in got.iter().zip(reference.vectors("query_mean")).enumerate() {
        assert_close(&format!("query {i}"), g, &w, 1e-5);
    }
    // The last text is longer than the model's 24 tokens.
    let long = texts.last().unwrap().as_str();
    assert_eq!(e.token_ids(None, long, &EmbedOptions::default()).unwrap().len(), 24);
    let strict = EmbedOptions { truncate: false, ..Default::default() };
    assert!(matches!(e.embed(None, &[long], &strict, &no_interrupt), Err(Error::Semantic(m)) if m.contains("24 tokens")));
    assert!(e.embed(None, &refs[..1], &strict, &no_interrupt).is_ok());
}

#[test]
fn cls_pooling_without_normalization() {
    let dir = TempDir::new("cls");
    let model = dir.add_fixture(
        "cls",
        Some("architecture = \"bert\"\ndimensions = 32\nmax_tokens = 24\npooling = \"cls\"\nnormalize = false\n"),
    );
    let reference = Reference::read(&fixture().join("reference.json"));
    let texts = reference.texts();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let got = embedder_for(&model, "cls", 4).embed(None, &refs, &EmbedOptions::default(), &no_interrupt).unwrap();
    for (i, (g, w)) in got.iter().zip(reference.vectors("cls")).enumerate() {
        assert_close(&format!("cls {i}"), g, &w, 1e-4);
    }
}

#[test]
fn models_load_lazily_once_and_are_evicted() {
    let dir = TempDir::new("cache");
    dir.add_fixture("a", None);
    dir.add_fixture("b", None);
    let config = EmbedderConfig { max_loaded_models: 1, threads: 1, ..Default::default() };
    let e = Arc::new(LocalEmbedder::new(Registry::scan(&dir.0).unwrap(), config).unwrap());
    let loaded = |e: &LocalEmbedder| e.models().into_iter().filter(|m| m.loaded).map(|m| m.name).collect::<Vec<_>>();
    assert!(loaded(&e).is_empty());
    assert!(matches!(e.embed(None, &["x"], &EmbedOptions::default(), &no_interrupt), Err(Error::Semantic(m)) if m.contains("no default")));
    assert!(matches!(e.embed(Some("c"), &["x"], &EmbedOptions::default(), &no_interrupt), Err(Error::Semantic(m)) if m.contains("installed: a, b")));

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let e = e.clone();
            std::thread::spawn(move || e.embed(Some("a"), &["graph nodes"], &EmbedOptions::default(), &no_interrupt).unwrap())
        })
        .collect();
    let first = threads.into_iter().map(|t| t.join().unwrap()).collect::<Vec<_>>();
    assert!(first.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(e.loads(), 1, "concurrent queries share one load");
    assert_eq!(loaded(&e), ["a"]);
    e.embed(Some("b"), &["x"], &EmbedOptions::default(), &no_interrupt).unwrap();
    assert_eq!(loaded(&e), ["b"], "only one model is kept");
    assert_eq!(e.loads(), 2);
}

#[test]
fn interrupt_is_checked_between_batches() {
    let e = embedder_for(&fixture(), "tiny-bert", 2);
    let texts = ["a", "b", "c", "d", "e"];
    let calls = AtomicUsize::new(0);
    e.embed(None, &texts, &EmbedOptions::default(), &|| {
        calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })
    .unwrap();
    assert!(calls.load(Ordering::Relaxed) >= 3, "once per batch of 2");
    let calls = AtomicUsize::new(0);
    let r = e.embed(None, &texts, &EmbedOptions::default(), &|| {
        if calls.fetch_add(1, Ordering::Relaxed) == 2 { Err(Error::Cancelled("stop".into())) } else { Ok(()) }
    });
    assert!(matches!(r, Err(Error::Cancelled(_))));
}

#[test]
fn from_cypher() {
    let reference = Reference::read(&fixture().join("reference.json"));
    let db = Database::in_memory();
    db.set_embedder(Arc::new(embedder_for(&fixture(), "tiny-bert", 32)));
    let r = db.execute("RETURN vector.embed('graph databases store nodes') AS v").unwrap();
    let Some(ResultValue::List(v)) = r.get(0, "v") else { panic!() };
    let got: Vec<f32> = v.iter().map(|x| match x {
        ResultValue::Float(f) => *f as f32,
        o => panic!("{o:?}"),
    }).collect();
    assert_close("cypher", &got, &reference.vectors("mean")[0], 1e-5);
    let r = db.execute("CALL vector.models() YIELD name, dimensions, maxTokens, loaded RETURN *").unwrap();
    assert_eq!(r.get(0, "name"), Some(&ResultValue::String("tiny-bert".into())));
    assert_eq!(r.get(0, "maxTokens"), Some(&ResultValue::Int(24)));
    assert_eq!(r.get(0, "loaded"), Some(&ResultValue::Bool(true)));
}

/// A real model; see the module docs.
#[test]
#[ignore = "needs GRAPHBUFF_EMBED_TEST_MODEL (a model folder with reference.json)"]
fn real_model_matches_transformers() {
    let dir = std::env::var("GRAPHBUFF_EMBED_TEST_MODEL").expect("set GRAPHBUFF_EMBED_TEST_MODEL");
    let dir = Path::new(&dir);
    let name = dir.file_name().unwrap().to_str().unwrap();
    check_model(dir, name, 1e-4);
}

#[test]
fn stats_and_load_callback() {
    let loads = Arc::new(AtomicUsize::new(0));
    let seen = loads.clone();
    let spec = ModelSpec::read("tiny-bert", &fixture()).unwrap();
    let e = LocalEmbedder::new(Registry::from_specs([spec]), EmbedderConfig::default()).unwrap().on_load(move |info| {
        assert_eq!((info.name, info.dimensions), ("tiny-bert", 32));
        assert!(info.bytes > 50_000);
        seen.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(e.stats(), graphbuff::EmbedStats::default());
    e.embed(None, &["graph nodes", "a"], &EmbedOptions::default(), &no_interrupt).unwrap();
    e.embed(None, &["vectors"], &EmbedOptions::default(), &no_interrupt).unwrap();
    let s = e.stats();
    assert_eq!((s.calls, s.texts, s.model_loads), (2, 3, 1));
    let tokens: usize = ["graph nodes", "a", "vectors"]
        .iter()
        .map(|t| e.token_ids(None, t, &EmbedOptions::default()).unwrap().len())
        .sum();
    assert_eq!(s.tokens, tokens as u64);
    assert!(s.seconds > 0.0 && s.load_seconds > 0.0);
    assert_eq!(loads.load(Ordering::Relaxed), 1);
}

#[test]
fn a_batch_is_charged_to_the_query_thread_while_it_runs() {
    let e = embedder_for(&fixture(), "tiny-bert", 32);
    e.embed(None, &["warm up"], &EmbedOptions::default(), &no_interrupt).unwrap();
    let before = graphbuff::accounting::thread_bytes();
    let charged = std::sync::Mutex::new(Vec::new());
    e.embed(None, &["graph databases store nodes", "a"], &EmbedOptions::default(), &|| {
        charged.lock().unwrap().push(graphbuff::accounting::thread_bytes() - before);
        Ok(())
    })
    .unwrap();
    let charged = charged.into_inner().unwrap();
    // [first check before tokenizing, the batch's check]
    assert!(charged.iter().any(|&b| b >= 4 * 2 * 3 * (6 * 32 + 2 * 64)), "{charged:?}");
    assert!(graphbuff::accounting::thread_bytes() - before < 4096, "the charge ends with the batch");
}
