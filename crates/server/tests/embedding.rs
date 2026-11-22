//! The real binary with `[embedding]`: models found in the `model` folder next to the
//! config file, `vector.embed()` over HTTP, the log lines and the `graphbuff_embed_*`
//! metrics. Without the `embed` feature, `vector.embed()` fails clearly.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;
#[cfg_attr(not(feature = "embed"), allow(unused_imports))]
use common::{toml_path, toml_str};

const BIN: &str = env!("CARGO_BIN_EXE_graphbuff-server");

fn start(config: &Path) -> (Child, String) {
    let mut cmd = Command::new(BIN);
    for key in ["GRAPHBUFF_TOKEN", "GRAPHBUFF_CONFIG", "GRAPHBUFF_ADMIN_USER", "GRAPHBUFF_ADMIN_PASSWORD_FILE"] {
        cmd.env_remove(key);
    }
    let mut child = cmd
        .arg("--config")
        .arg(config)
        .env("GRAPHBUFF_ADMIN_PASSWORD", "admin-password")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let addr = line.split("http://").nth(1).and_then(|r| r.split_whitespace().next()).unwrap_or_else(|| panic!("{line:?}"));
    (child, addr.to_string())
}

fn request(addr: &str, method: &str, path: &str, body: &str) -> String {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        conn,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer secret\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    reply
}

fn query(addr: &str, q: &str) -> String {
    request(addr, "POST", "/query", &format!("{{\"query\": {q:?}}}"))
}

fn stop(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

#[cfg(feature = "embed")]
#[test]
fn models_next_to_the_config_file() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../embed/tests/fixtures/tiny-bert");
    let logs = std::env::temp_dir().join(format!("graphbuff-embed-logs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&logs);
    let config = common::server_config(&[
        ("security.token", toml_str("secret")),
        ("logs.dir", toml_path(&logs)),
        ("embedding.threads", "1".into()),
    ]);
    let models = config.parent().unwrap().join("model");
    for (name, toml) in [("tiny", None), ("broken", Some("architecture = \"gpt\"\ndimensions = 32\n"))] {
        let to = models.join(name);
        std::fs::create_dir_all(&to).unwrap();
        for f in ["config.json", "tokenizer.json", "model.safetensors", "model.toml"] {
            std::fs::copy(fixture.join(f), to.join(f)).unwrap();
        }
        if let Some(t) = toml {
            std::fs::write(to.join("model.toml"), t).unwrap();
        }
    }

    let (child, addr) = start(&config);
    let result = std::panic::catch_unwind(|| {
        let r = query(&addr, "RETURN size(vector.embed('graph databases store nodes')) AS n");
        assert!(r.starts_with("HTTP/1.1 200") && r.contains("32"), "{r}");
        let r = query(&addr, "CALL vector.embedBatch(['a', 'b'], 'tiny') YIELD index RETURN count(index) AS n");
        assert!(r.contains('2'), "{r}");
        let r = query(&addr, "RETURN vector.embed('x', 'broken') AS v");
        assert!(r.contains("unknown embedding model 'broken'"), "{r}");
        let m = request(&addr, "GET", "/metrics", "");
        for line in [
            "graphbuff_embed_calls_total 2",
            "graphbuff_embed_texts_total 3",
            "graphbuff_embed_model_loads_total 1",
            "graphbuff_embed_model_loaded{model=\"tiny\"} 1",
        ] {
            assert!(m.contains(line), "{line:?} not in {m}");
        }
        let s = query(&addr, "SHOW SETTINGS YIELD name, value WHERE name = 'embedding.threads' RETURN value");
        assert!(s.contains("\"1\""), "{s}");
    });
    stop(child);
    result.unwrap();
    let log = std::fs::read_to_string(logs.join("graphbuff.log")).unwrap();
    for line in [
        "embedding model `broken` in ",
        "is skipped: model.toml: unknown architecture `gpt`",
        "embedding models in ",
        "tiny (32 dimensions); default: tiny; device: cpu",
        "loaded embedding model `tiny` (32 dimensions, 0.1 MiB of weights) in ",
    ] {
        assert!(log.contains(line), "{line:?} not in {log}");
    }
    let _ = std::fs::remove_dir_all(&logs);
}

#[cfg(feature = "embed")]
#[test]
fn a_named_model_folder_must_exist() {
    let config = common::server_config(&[
        ("security.token", toml_str("secret")),
        ("embedding.model_dir", toml_str("no-such-folder")),
    ]);
    let out = Command::new(BIN)
        .arg("--config")
        .arg(&config)
        .env("GRAPHBUFF_ADMIN_PASSWORD", "admin-password")
        .env_remove("GRAPHBUFF_CONFIG")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(all.contains("embedding: the model folder") && all.contains("does not exist"), "{all}");
}

#[test]
fn without_models_vector_embed_fails_clearly() {
    let config = common::server_config(&[("security.token", toml_str("secret"))]);
    let (child, addr) = start(&config);
    let r = query(&addr, "RETURN vector.embed('x') AS v");
    let m = request(&addr, "GET", "/metrics", "");
    stop(child);
    assert!(r.contains("embedding is not available"), "{r}");
    assert!(!m.contains("graphbuff_embed_"), "no embedding metrics without models");
}
