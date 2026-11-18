//! `graphbuff-server --demo`: the real binary, started with the embedded movies graph.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_graphbuff-server");

#[test]
fn demo_movies_is_loaded_at_startup() {
    let mut child = Command::new(BIN)
        .args(["--bind", "127.0.0.1:0", "--in-memory", "--users", "off", "--demo", "movies"])
        .env_remove("GRAPHBUFF_TOKEN")
        .env_remove("GRAPHBUFF_USERS")
        .env("GRAPHBUFF_CONFIG", "-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut listening = String::new();
    out.read_line(&mut listening).unwrap();
    let mut loaded = String::new();
    out.read_line(&mut loaded).unwrap();

    let result = std::panic::catch_unwind(|| {
        assert_eq!(loaded.trim_end(), "demo `movies` loaded: 7 nodes, 8 relationships");
        let addr = listening
            .split("http://")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("no address in {listening:?}"));
        let body = r#"{"query": "MATCH (m:Movie) RETURN m.title AS title ORDER BY title"}"#;
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            conn,
            "POST /query HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        for title in ["John Wick", "The Matrix", "The Matrix Reloaded"] {
            assert!(reply.contains(title), "{reply}");
        }
    });
    child.kill().unwrap();
    child.wait().unwrap();
    result.unwrap();
}

#[test]
fn unknown_demo_is_refused() {
    let out = Command::new(BIN).args(["--demo", "nope"]).env("GRAPHBUFF_CONFIG", "-").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown demo `nope`; available: movies"), "{err}");
}
