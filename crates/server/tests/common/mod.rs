//! Config files for the tests that run the real binary.

#![allow(dead_code)] // each test file uses some of these

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A TOML string for `path` (a literal string: no escapes, so Windows paths stay as they are).
pub fn toml_path(path: impl AsRef<Path>) -> String {
    format!("'{}'", path.as_ref().display())
}

/// A TOML string.
pub fn toml_str(s: &str) -> String {
    format!("{s:?}")
}

/// TOML for `settings` (`("section.key", value as TOML)`), grouped by section.
pub fn toml(settings: &[(&str, String)]) -> String {
    let mut sections: Vec<(&str, Vec<String>)> = Vec::new();
    for (name, value) in settings {
        let (section, key) = name.split_once('.').expect("section.key");
        let line = format!("{key} = {value}");
        match sections.iter_mut().find(|(s, _)| *s == section) {
            Some((_, lines)) => lines.push(line),
            None => sections.push((section, vec![line])),
        }
    }
    sections.iter().map(|(s, lines)| format!("[{s}]\n{}\n", lines.join("\n"))).collect::<Vec<_>>().join("\n")
}

/// Writes `text` to a new config file and returns its path.
pub fn config_file(text: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "graphbuff-config-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("graphbuff.toml");
    std::fs::write(&file, text).unwrap();
    file
}

/// The config file of a test server: any free port, logs to standard error and memory
/// storage, unless `settings` (as for [`toml`]) say otherwise; with `storage.data`, the
/// default storage mode.
pub fn server_config(settings: &[(&str, String)]) -> PathBuf {
    let has = |k: &str| settings.iter().any(|(n, _)| *n == k);
    let mut all: Vec<(&str, String)> = Vec::new();
    if !has("server.bind") {
        all.push(("server.bind", toml_str("127.0.0.1:0")));
    }
    if !has("storage.data") && !has("storage.mode") {
        all.push(("storage.mode", toml_str("memory")));
    }
    if !has("logs.dir") {
        all.push(("logs.dir", toml_str("-")));
    }
    all.extend(settings.iter().cloned());
    config_file(&toml(&all))
}

/// [`server_config`] with the data folder `data` (memory storage without one).
pub fn data_config(data: Option<&str>) -> PathBuf {
    match data {
        Some(d) => server_config(&[("storage.data", toml_path(d))]),
        None => server_config(&[]),
    }
}
