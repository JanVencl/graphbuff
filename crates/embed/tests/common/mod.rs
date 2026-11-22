#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::json::{self, Json};

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-bert")
}

/// A temporary model folder, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-embed-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// Copies the fixture model to `<dir>/<name>`, with `manifest` as its model.toml if given.
    pub fn add_fixture(&self, name: &str, manifest: Option<&str>) -> PathBuf {
        let to = self.0.join(name);
        fs::create_dir_all(&to).unwrap();
        for f in ["config.json", "tokenizer.json", "model.safetensors", "model.toml"] {
            fs::copy(fixture().join(f), to.join(f)).unwrap();
        }
        if let Some(m) = manifest {
            fs::write(to.join("model.toml"), m).unwrap();
        }
        to
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The reference file written by the fixture or reference scripts.
pub struct Reference(pub Json);

impl Reference {
    pub fn read(file: &Path) -> Reference {
        Reference(json::parse(&fs::read_to_string(file).unwrap()).unwrap())
    }

    fn field(&self, key: &str) -> &Vec<Json> {
        match &self.0 {
            Json::Object(o) => match o.get(key) {
                Some(Json::Array(a)) => a,
                other => panic!("{key}: {other:?}"),
            },
            _ => panic!("not an object"),
        }
    }

    pub fn has(&self, key: &str) -> bool {
        matches!(&self.0, Json::Object(o) if o.contains_key(key))
    }

    pub fn texts(&self) -> Vec<String> {
        self.field("texts").iter().map(|t| match t {
            Json::String(s) => s.clone(),
            o => panic!("{o:?}"),
        }).collect()
    }

    pub fn ids(&self, key: &str) -> Vec<Vec<u32>> {
        self.field(key)
            .iter()
            .map(|r| match r {
                Json::Array(a) => a.iter().map(|x| match x {
                    Json::Int(i) => *i as u32,
                    o => panic!("{o:?}"),
                }).collect(),
                o => panic!("{o:?}"),
            })
            .collect()
    }

    pub fn vectors(&self, key: &str) -> Vec<Vec<f64>> {
        self.field(key)
            .iter()
            .map(|r| match r {
                Json::Array(a) => a.iter().map(|x| match x {
                    Json::Float(f) => *f,
                    Json::Int(i) => *i as f64,
                    o => panic!("{o:?}"),
                }).collect(),
                o => panic!("{o:?}"),
            })
            .collect()
    }
}

/// Fails unless `got` is `want` to within `tol` per component (and cosine ≥ 0.9999).
pub fn assert_close(what: &str, got: &[f32], want: &[f64], tol: f64) {
    assert_eq!(got.len(), want.len(), "{what}: dimensions");
    let max = got.iter().zip(want).map(|(g, w)| (*g as f64 - w).abs()).fold(0.0, f64::max);
    let dot: f64 = got.iter().zip(want).map(|(g, w)| *g as f64 * w).sum();
    let ng = got.iter().map(|g| (*g as f64).powi(2)).sum::<f64>().sqrt();
    let nw = want.iter().map(|w| w * w).sum::<f64>().sqrt();
    let cos = dot / (ng * nw).max(1e-30);
    assert!(max <= tol && cos >= 0.9999, "{what}: max |diff| {max:e}, cosine {cos}");
}
