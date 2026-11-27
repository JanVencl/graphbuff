//! Vectors against Hugging Face `transformers`: the tiny random BERT fixture always (its
//! reference comes from scripts/make_fixture.py), a real model when one is given:
//!
//! ```text
//! python scripts/reference.py path/to/model/minilm      # writes reference.json there
//! GRAPHBUFF_EMBED_TEST_MODEL=path/to/model/minilm cargo test -p graphbuff-embed --release -- --ignored
//! ```

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Reference, TempDir, assert_close, fixture, fixture_named};
use graphbuff::{Database, EmbedOptions, EmbedTask, Embedder, Error, ResultValue};
use graphbuff_embed::{EmbedderConfig, LocalEmbedder, Matryoshka, ModelSpec, Registry};

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

/// Token ids and vectors with the query prefix, and the cut at the model's 24 tokens.
fn check_query(dir: &Path, name: &str, tol: f64) {
    let reference = Reference::read(&dir.join("reference.json"));
    let texts = reference.texts();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let e = embedder_for(dir, name, 32);
    let query = EmbedOptions { task: EmbedTask::Query, ..Default::default() };
    for (i, ids) in reference.ids("query_ids").iter().enumerate() {
        assert_eq!(&e.token_ids(None, &texts[i], &query).unwrap(), ids);
    }
    let got = e.embed(None, &refs, &query, &no_interrupt).unwrap();
    for (i, (g, w)) in got.iter().zip(reference.vectors("query_mean")).enumerate() {
        assert_close(&format!("query {i}"), g, &w, tol);
    }
    // The last text is longer than the model's 24 tokens.
    let long = texts.last().unwrap().as_str();
    assert_eq!(e.token_ids(None, long, &EmbedOptions::default()).unwrap().len(), 24);
    let strict = EmbedOptions { truncate: false, ..Default::default() };
    assert!(matches!(e.embed(None, &[long], &strict, &no_interrupt), Err(Error::Semantic(m)) if m.contains("24 tokens")));
    assert!(e.embed(None, &refs[..1], &strict, &no_interrupt).is_ok());
}

/// Chunks of a long text fit the model with either prefix, and their token counts are
/// what the model gives them; `embedChunks` embeds each as `embed` would.
fn check_chunks(dir: &Path, name: &str) {
    let e = Arc::new(embedder_for(dir, name, 8));
    let db = Database::in_memory();
    db.set_embedder(e.clone());
    let base = "Graph databases store nodes and relationships. Příliš žluťoučký kůň úpěl ďábelské ódy!\n\n\
                Vector indexes find similar embeddings quickly; a model turns text into vectors. ";
    let max = e.models()[0].max_tokens;
    // Long enough for more than four chunks of the model's size.
    let per = e.token_spans(None, base, EmbedTask::Document).unwrap().spans.len();
    let text = base.repeat(4 * max / per + 2);
    let specials = e.token_ids(None, "", &EmbedOptions::default()).unwrap().len();
    for task in ["document", "query"] {
        let opts = EmbedOptions { task: if task == "query" { EmbedTask::Query } else { EmbedTask::Document }, truncate: false, dimensions: None };
        let fit = e.token_spans(None, &text, opts.task).unwrap().fit;
        assert!(fit + specials <= max, "{name}: fit {fit}");
        for (size, overlap) in [(fit, 0), (fit / 2, fit / 8)] {
            let params = [
                ("t".to_string(), graphbuff::Value::String(text.clone())),
                ("o".to_string(), graphbuff::Value::Map([
                    ("task".to_string(), graphbuff::Value::String(task.into())),
                    ("size".to_string(), graphbuff::Value::Int(size as i64)),
                    ("overlap".to_string(), graphbuff::Value::Int(overlap as i64)),
                ].into_iter().collect())),
            ].into_iter().collect();
            let r = db.execute_with_params("CALL vector.embedChunks($t, null, $o) YIELD text, tokens, vector RETURN *", &params).unwrap();
            assert!(r.len() > 3, "{name}: {} chunks", r.len());
            let mut pieces = Vec::new();
            for i in 0..r.len() {
                let (Some(ResultValue::String(piece)), Some(ResultValue::Int(tokens))) = (r.get(i, "text"), r.get(i, "tokens")) else { panic!() };
                let ids = e.token_ids(None, piece, &opts).unwrap_or_else(|err| panic!("{name} {task}: {piece:?}: {err}"));
                assert!(ids.len() <= max, "{name}: {} tokens: {piece:?}", ids.len());
                assert!(*tokens as usize <= size, "{name}: {tokens} > {size}");
                let own = e.token_spans(None, piece, opts.task).unwrap();
                assert_eq!(own.spans.len(), *tokens as usize, "{name}: {piece:?}");
                assert_eq!(ids.len(), specials + (max - specials - own.fit) + own.spans.len(), "{name}: prefix, text and specials add up");
                pieces.push(piece.clone());
            }
            let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
            let want = e.embed(None, &refs, &opts, &no_interrupt).unwrap();
            for (i, w) in want.iter().enumerate() {
                let Some(ResultValue::List(v)) = r.get(i, "vector") else { panic!() };
                let got: Vec<f32> = v.iter().map(|x| match x { ResultValue::Float(f) => *f as f32, o => panic!("{o:?}") }).collect();
                assert_close(&format!("{name} chunk {i}"), &got, &w.iter().map(|x| *x as f64).collect::<Vec<_>>(), 1e-5);
            }
        }
    }
}

#[test]
fn chunks_fit_the_fixtures() {
    for name in ["tiny-bert", "tiny-xlmr", "tiny-nomic"] {
        check_chunks(&fixture_named(name), name);
    }
}

/// `model`'s weights written again as `dtype` (F16 or BF16) into `<dir>/<name>`, with its
/// other files; and, as `<name>-f32`, the same rounded weights in F32.
fn half_copy(model: &Path, dir: &TempDir, name: &str, dtype: safetensors::Dtype) -> (PathBuf, PathBuf) {
    use burn::tensor::{bf16, f16};
    use safetensors::tensor::{SafeTensors, TensorView};
    let bytes = std::fs::read(model.join("model.safetensors")).unwrap();
    let st = SafeTensors::deserialize(&bytes).unwrap();
    let mut half = Vec::new();
    let mut rounded = Vec::new();
    for (key, view) in st.tensors() {
        if view.dtype() != safetensors::Dtype::F32 {
            // Not weights (`position_ids`, I64): as they are.
            half.push((key.clone(), view.dtype(), view.shape().to_vec(), view.data().to_vec()));
            rounded.push((key, view.dtype(), view.shape().to_vec(), view.data().to_vec()));
            continue;
        }
        let floats: Vec<f32> = view.data().as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        let (h, r): (Vec<u8>, Vec<f32>) = match dtype {
            safetensors::Dtype::F16 => {
                let v: Vec<f16> = floats.iter().map(|&x| f16::from_f32(x)).collect();
                (v.iter().flat_map(|x| x.to_le_bytes()).collect(), v.iter().map(|x| x.to_f32()).collect())
            }
            safetensors::Dtype::BF16 => {
                let v: Vec<bf16> = floats.iter().map(|&x| bf16::from_f32(x)).collect();
                (v.iter().flat_map(|x| x.to_le_bytes()).collect(), v.iter().map(|x| x.to_f32()).collect())
            }
            other => panic!("{other:?}"),
        };
        half.push((key.clone(), dtype, view.shape().to_vec(), h));
        rounded.push((key, safetensors::Dtype::F32, view.shape().to_vec(), r.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()));
    }
    let write = |to: &str, tensors: Vec<(String, TensorView<'_>)>| {
        let to = dir.0.join(to);
        std::fs::create_dir_all(&to).unwrap();
        for f in ["config.json", "tokenizer.json", "model.toml", "reference.json"] {
            std::fs::copy(model.join(f), to.join(f)).unwrap();
        }
        safetensors::serialize_to_file(tensors, None, &to.join("model.safetensors")).unwrap();
        to
    };
    let h = write(name, half.iter().map(|(k, d, s, b)| (k.clone(), TensorView::new(*d, s.clone(), b).unwrap())).collect());
    let f32_name = format!("{name}-f32");
    let r = write(&f32_name, rounded.iter().map(|(k, d, s, b)| (k.clone(), TensorView::new(*d, s.clone(), b).unwrap())).collect());
    (h, r)
}

/// `model` with F16 and BF16 weights: they load as the F32 values they round to, and the
/// vectors stay within `tol` (F16, BF16) of the F32 model's.
fn check_half(model: &Path, name: &str, tol: [f64; 2]) {
    let dir = TempDir::new("half");
    let reference = Reference::read(&model.join("reference.json"));
    let texts = reference.texts();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let want = reference.vectors(if reference.has("mean") { "mean" } else { "vectors" });
    for (dtype, tol) in [safetensors::Dtype::F16, safetensors::Dtype::BF16].into_iter().zip(tol) {
        let name = format!("{name}-{dtype:?}").to_lowercase();
        let (half, rounded) = half_copy(model, &dir, &name, dtype);
        let got = embedder_for(&half, &name, 32).embed(None, &refs, &EmbedOptions::default(), &no_interrupt).unwrap();
        let exact = embedder_for(&rounded, "r", 32).embed(None, &refs, &EmbedOptions::default(), &no_interrupt).unwrap();
        assert_eq!(got, exact, "{name}: widened exactly");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            let max = g.iter().zip(w).map(|(g, w)| (*g as f64 - w).abs()).fold(0.0, f64::max);
            assert!(max <= tol, "{name}, text {i}: max |diff| {max:e} from the F32 model");
        }
    }
}

#[test]
fn half_precision_weights() {
    for name in ["tiny-bert", "tiny-xlmr", "tiny-nomic"] {
        check_half(&fixture_named(name), name, [2e-3, 1.5e-2]);
    }
}

#[test]
fn tiny_bert_matches_transformers() {
    check_model(&fixture(), "tiny-bert", 1e-5);
}

#[test]
fn tiny_xlm_roberta_matches_transformers() {
    check_model(&fixture_named("tiny-xlmr"), "tiny-xlmr", 1e-5);
    check_query(&fixture_named("tiny-xlmr"), "tiny-xlmr", 1e-5);
}

/// Against Nomic's own model code (scripts/make_fixture.py nomic-bert).
#[test]
fn tiny_nomic_bert_matches_its_model_code() {
    check_model(&fixture_named("tiny-nomic"), "tiny-nomic", 1e-5);
    check_query(&fixture_named("tiny-nomic"), "tiny-nomic", 1e-5);
}

/// Fewer dimensions from a Matryoshka model: the full vector unchanged, the cut one as
/// the manifest says (from the same model's unnormalized vector), and no cut for a model
/// without `matryoshka`.
#[test]
fn matryoshka_cuts() {
    let dir = TempDir::new("matryoshka");
    let base = "architecture = \"nomic-bert\"\ndimensions = 32\nmax_tokens = 24\nquery_prefix = \"query: \"\n";
    let copy = |name: &str, extra: &str| {
        let to = dir.0.join(name);
        std::fs::create_dir_all(&to).unwrap();
        for f in ["config.json", "tokenizer.json", "model.safetensors"] {
            std::fs::copy(fixture_named("tiny-nomic").join(f), to.join(f)).unwrap();
        }
        std::fs::write(to.join("model.toml"), format!("{base}{extra}")).unwrap();
        to
    };
    let raw = embedder_for(&copy("raw", "normalize = false\n"), "raw", 8);
    let texts = Reference::read(&fixture_named("tiny-nomic").join("reference.json")).texts();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let opts = |d: Option<usize>| EmbedOptions { dimensions: d, ..Default::default() };
    let pooled = raw.embed(None, &refs, &opts(None), &no_interrupt).unwrap();
    let reference = Reference::read(&fixture_named("tiny-nomic").join("reference.json"));
    let want = reference.vectors(if reference.has("mean") { "mean" } else { "vectors" });
    for (how, name) in [(Matryoshka::LayerNorm, "layer-norm"), (Matryoshka::Truncate, "truncate")] {
        let e = embedder_for(&copy(name, &format!("matryoshka = \"{name}\"\n")), name, 8);
        assert!(e.models()[0].matryoshka);
        let full = e.embed(None, &refs, &opts(None), &no_interrupt).unwrap();
        let same = e.embed(None, &refs, &opts(Some(32)), &no_interrupt).unwrap();
        for (i, w) in want.iter().enumerate() {
            assert_close(&format!("{name}: full {i}"), &full[i], w, 1e-5);
            assert_close(&format!("{name}: 32 {i}"), &same[i], w, 1e-5);
        }
        for dims in [16, 8, 1] {
            let got = e.embed(None, &refs, &opts(Some(dims)), &no_interrupt).unwrap();
            for (i, g) in got.iter().enumerate() {
                let w: Vec<f64> = how.cut(pooled[i].clone(), dims, true).into_iter().map(f64::from).collect();
                assert_eq!(g.len(), dims);
                assert_close(&format!("{name}: {dims} {i}"), g, &w, 1e-5);
                assert!((g.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5, "normalized again");
            }
        }
        let e33 = e.embed(None, &refs[..1], &opts(Some(33)), &no_interrupt).unwrap_err().to_string();
        assert!(e33.contains("32 dimensions at most"), "{e33}");
    }
    let plain = embedder_for(&copy("plain", ""), "plain", 8);
    assert!(!plain.models()[0].matryoshka);
    assert_eq!(plain.embed(None, &refs[..1], &opts(Some(32)), &no_interrupt).unwrap()[0].len(), 32);
    let e = plain.embed(None, &refs[..1], &opts(Some(16)), &no_interrupt).unwrap_err().to_string();
    assert!(e.contains("not a Matryoshka model"), "{e}");

    // From Cypher, and an index of the cut length may name the model.
    let db = Database::in_memory();
    db.set_embedder(Arc::new(embedder_for(&dir.0.join("layer-norm"), "layer-norm", 8)));
    let r = db.execute("RETURN size(vector.embed('graph', null, {dimensions: 8})) AS d").unwrap();
    assert_eq!(r.get(0, "d"), Some(&ResultValue::Int(8)));
    db.execute("CREATE VECTOR INDEX short FOR (d:Doc) ON d.e OPTIONS {indexConfig: {`vector.dimensions`: 8, `vector.embedding_model`: 'layer-norm'}}")
        .unwrap();
    let err = db
        .execute("CREATE VECTOR INDEX long FOR (d:Doc) ON d.f OPTIONS {indexConfig: {`vector.dimensions`: 64, `vector.embedding_model`: 'layer-norm'}}")
        .unwrap_err()
        .to_string();
    assert!(err.contains("at most"), "{err}");
}

/// `rotary_scaling` on tiny-nomic (trained on 24 tokens, 64 positions): texts up to 24
/// tokens embed as without it, longer ones up to 64 tokens are taken, and a long text's
/// vector does not depend on the texts batched with it.
#[test]
fn rotary_scaling_past_the_trained_length() {
    let dir = TempDir::new("rotary");
    let to = dir.0.join("long");
    std::fs::create_dir_all(&to).unwrap();
    for f in ["config.json", "tokenizer.json", "model.safetensors"] {
        std::fs::copy(fixture_named("tiny-nomic").join(f), to.join(f)).unwrap();
    }
    std::fs::write(
        to.join("model.toml"),
        "architecture = \"nomic-bert\"\ndimensions = 32\nrotary_scaling = 2\nquery_prefix = \"query: \"\n",
    )
    .unwrap();
    let e = embedder_for(&to, "long", 32);
    assert_eq!(e.models()[0].max_tokens, 64, "n_positions");
    let opts = EmbedOptions::default();

    // Up to the trained length: the reference vectors.
    let reference = Reference::read(&fixture_named("tiny-nomic").join("reference.json"));
    let texts = reference.texts();
    let want = reference.vectors("mean");
    let short: Vec<&str> = texts.iter().map(String::as_str).filter(|t| e.token_ids(None, t, &opts).unwrap().len() <= 24).collect();
    assert!(short.len() >= 5);
    for (i, v) in e.embed(None, &short, &opts, &no_interrupt).unwrap().iter().enumerate() {
        let j = texts.iter().position(|t| t == short[i]).unwrap();
        assert_close(&format!("short {i}"), v, &want[j], 1e-5);
    }

    // Past it: up to 64 tokens, each with its own base.
    let long: Vec<String> = [6, 10, 20].iter().map(|n| vec!["graph nodes and vectors"; *n].join(" ")).collect();
    let lens: Vec<usize> = long.iter().map(|t| e.token_ids(None, t, &opts).unwrap().len()).collect();
    assert!(lens[0] > 24 && lens[1] > lens[0] && lens[2] == 64, "{lens:?}");
    let alone: Vec<Vec<f32>> = long.iter().map(|t| e.embed(None, &[t], &opts, &no_interrupt).unwrap().remove(0)).collect();
    let mut mixed: Vec<&str> = long.iter().map(String::as_str).collect();
    mixed.extend(&short);
    let together = e.embed(None, &mixed, &opts, &no_interrupt).unwrap();
    for i in 0..long.len() {
        let w: Vec<f64> = alone[i].iter().map(|x| *x as f64).collect();
        assert_close(&format!("long {i} in a batch"), &together[i], &w, 1e-5);
        assert!((alone[i].iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
    }
    for (i, v) in together[long.len()..].iter().enumerate() {
        let j = texts.iter().position(|t| t == short[i]).unwrap();
        assert_close(&format!("short {i} next to long ones"), v, &want[j], 1e-5);
    }
    // Without it the model stops at 24 tokens.
    std::fs::write(to.join("model.toml"), "architecture = \"nomic-bert\"\ndimensions = 32\n").unwrap();
    assert_eq!(embedder_for(&to, "long", 32).models()[0].max_tokens, 24);
}

#[test]
fn query_prefix_and_truncation() {
    check_query(&fixture(), "tiny-bert", 1e-5);
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
    check_chunks(dir, name);
}

/// Writes two copies of the model's weights (F16 or BF16, and rounded F32) to the temp
/// folder: point TMP or TMPDIR at a disk with room.
#[test]
#[ignore = "needs GRAPHBUFF_EMBED_TEST_MODEL (a model folder with reference.json)"]
fn real_model_in_half_precision() {
    let dir = std::env::var("GRAPHBUFF_EMBED_TEST_MODEL").expect("set GRAPHBUFF_EMBED_TEST_MODEL");
    let dir = Path::new(&dir);
    check_half(dir, dir.file_name().unwrap().to_str().unwrap(), [2e-3, 1e-2]);
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

/// The GPU (wgpu) gives the same vectors, to float noise. Needs a GPU, so not run by default.
#[cfg(feature = "gpu")]
#[test]
#[ignore = "needs a GPU: cargo test -p graphbuff-embed --features gpu -- --ignored gpu"]
fn gpu_matches_transformers() {
    for name in ["tiny-bert", "tiny-xlmr", "tiny-nomic"] {
        let dir = fixture_named(name);
        let reference = Reference::read(&dir.join("reference.json"));
        let texts = reference.texts();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let spec = ModelSpec::read(name, &dir).unwrap();
        let config = EmbedderConfig { device: graphbuff_embed::DeviceKind::Gpu, ..Default::default() };
        let e = LocalEmbedder::new(Registry::from_specs([spec]), config).unwrap();
        let got = e.embed(None, &refs, &EmbedOptions::default(), &no_interrupt).unwrap();
        for (i, (g, w)) in got.iter().zip(reference.vectors("mean")).enumerate() {
            assert_close(&format!("gpu {name} {i}"), g, &w, 1e-4);
        }
    }
}
