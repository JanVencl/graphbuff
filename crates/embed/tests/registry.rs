//! Scanning a model folder: good models are found, bad ones skipped with a reason.

mod common;

use std::fs;

use common::TempDir;
use graphbuff_embed::{EmbedderConfig, LocalEmbedder, Pooling, Registry};

const OK: &str = "architecture = \"bert\"\ndimensions = 32\n";

#[test]
fn good_and_bad_models() {
    let dir = TempDir::new("scan");
    dir.add_fixture("tiny", None);
    dir.add_fixture("tiny-cls", Some("architecture = \"bert\"\ndimensions = 32\npooling = \"cls\"\nnormalize = false\n"));
    dir.add_fixture("Upper", Some(OK));
    dir.add_fixture("wrong-dims", Some("architecture = \"bert\"\ndimensions = 384\n"));
    dir.add_fixture("too-long", Some("architecture = \"bert\"\ndimensions = 32\nmax_tokens = 512\n"));
    dir.add_fixture("bad-toml", Some("architecture = bert\n"));
    let no_weights = dir.add_fixture("no-weights", Some(OK));
    fs::remove_file(no_weights.join("model.safetensors")).unwrap();
    let no_manifest = dir.add_fixture("no-manifest", None);
    fs::remove_file(no_manifest.join("model.toml")).unwrap();
    let bad_config = dir.add_fixture("bad-config", Some(OK));
    fs::write(bad_config.join("config.json"), "{\"hidden_size\": 32}").unwrap();
    fs::write(dir.0.join("README.txt"), "files are ignored").unwrap();

    let r = Registry::scan(&dir.0).unwrap();
    let names: Vec<&str> = r.models().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["tiny", "tiny-cls"]);
    assert_eq!(r.get("tiny-cls").unwrap().manifest.pooling, Pooling::Cls);
    assert_eq!(r.get("tiny").unwrap().max_tokens(), 24);
    let reason = |n: &str| r.skipped().iter().find(|s| s.name == n).map(|s| s.reason.clone()).unwrap_or_else(|| panic!("{n} not skipped"));
    assert!(reason("Upper").contains("not a valid model name"));
    assert!(reason("wrong-dims").contains("384 dimensions"));
    assert!(reason("too-long").contains("max_position_embeddings"));
    assert!(reason("bad-toml").contains("model.toml"));
    assert!(reason("no-weights").contains("model.safetensors is missing"));
    assert!(reason("no-manifest").contains("model.toml"));
    assert!(reason("bad-config").contains("config.json"));
    assert_eq!(r.skipped().len(), 7);
}

#[test]
fn a_missing_folder_is_an_error_and_an_empty_one_has_no_models() {
    let dir = TempDir::new("empty");
    assert!(Registry::scan(&dir.0.join("nope")).is_err());
    let r = Registry::scan(&dir.0).unwrap();
    assert!(r.is_empty() && r.skipped().is_empty());
}

#[test]
fn default_model() {
    let dir = TempDir::new("default");
    dir.add_fixture("one", None);
    let e = LocalEmbedder::new(Registry::scan(&dir.0).unwrap(), EmbedderConfig::default()).unwrap();
    assert_eq!(e.default_model(), Some("one"), "the only model is the default");
    dir.add_fixture("two", None);
    let r = Registry::scan(&dir.0).unwrap();
    let e = LocalEmbedder::new(r.clone(), EmbedderConfig::default()).unwrap();
    assert_eq!(e.default_model(), None, "no default among several");
    let config = EmbedderConfig { default_model: Some("two".into()), ..Default::default() };
    assert_eq!(LocalEmbedder::new(r.clone(), config).unwrap().default_model(), Some("two"));
    let config = EmbedderConfig { default_model: Some("three".into()), ..Default::default() };
    assert!(LocalEmbedder::new(r, config).unwrap_err().contains("`three` is not installed"));
}
