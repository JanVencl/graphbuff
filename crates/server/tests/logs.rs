//! The real binary: the data directory from GRAPHBUFF_DATA, the logs under it, and what
//! they record (start-up, failed logins, slow queries).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_graphbuff-server");

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphbuff-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Starts the server with `args` and the environment `env`; returns it and its address.
fn start(args: &[&str], env: &[(&str, &Path)]) -> (Child, String) {
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0"]).args(args);
    cmd.env("GRAPHBUFF_CONFIG", "-");
    for key in ["GRAPHBUFF_TOKEN", "GRAPHBUFF_USERS", "GRAPHBUFF_DATA", "GRAPHBUFF_LOG_DIR", "GRAPHBUFF_LOG_LEVEL"] {
        cmd.env_remove(key);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut listening = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut listening).unwrap();
    let addr = listening
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no address in {listening:?}"))
        .to_string();
    (child, addr)
}

/// Sends a query; returns the status line.
fn query(addr: &str, auth: Option<&str>, body: &str) -> String {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = auth.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    write!(
        conn,
        "POST /query HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    reply.lines().next().unwrap_or_default().to_string()
}

fn stop(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn data_and_logs_go_to_graphbuff_data() {
    let data = temp_dir("data-env");
    let (child, addr) = start(&["--slow-query", "0", "--token", "secret"], &[("GRAPHBUFF_DATA", &data)]);
    let result = std::panic::catch_unwind(|| {
        let ok = query(&addr, Some("Bearer secret"), r#"{"query": "CREATE (:Person {name: 'Ann'})"}"#);
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let denied = query(&addr, Some("Bearer wrong"), r#"{"query": "RETURN 1"}"#);
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
    });
    stop(child);
    result.unwrap();

    assert!(data.join("catalog").exists(), "no catalog in {}", data.display());
    let events = std::fs::read_to_string(data.join("logs/graphbuff.log")).unwrap();
    assert!(events.contains(" INFO  graphbuff-server "), "{events}");
    assert!(events.contains(&format!("data in {}", data.display())), "{events}");
    assert!(events.contains(" WARN  failed login (http) from 127.0.0.1:"), "{events}");
    let queries = std::fs::read_to_string(data.join("logs/query.log")).unwrap();
    assert!(queries.contains(" http graphbuff token CREATE (:Person {name: 'Ann'})"), "{queries}");
    assert!(!queries.contains("RETURN 1"), "{queries}");

    // The data is still there after a restart.
    let (child, addr) = start(&["--token", "secret"], &[("GRAPHBUFF_DATA", &data)]);
    let result = std::panic::catch_unwind(|| {
        let mut conn = TcpStream::connect(&addr).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let body = r#"{"query": "MATCH (p:Person) RETURN p.name AS name"}"#;
        write!(
            conn,
            "POST /query HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer secret\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).unwrap();
        assert!(reply.contains("Ann"), "{reply}");
    });
    stop(child);
    result.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn log_dir_can_be_elsewhere_and_in_memory_writes_no_files() {
    let data = temp_dir("data-flag");
    let logs = temp_dir("logs-flag");
    let (child, _) = start(
        &["--data", data.to_str().unwrap(), "--log-dir", logs.to_str().unwrap(), "--log-level", "warn", "--users", "off"],
        &[],
    );
    stop(child);
    assert!(logs.join("graphbuff.log").exists());
    assert!(!data.join("logs").exists());
    // At level warn, the start-up lines are not written.
    assert_eq!(std::fs::read_to_string(logs.join("graphbuff.log")).unwrap(), "");

    let empty = temp_dir("in-memory");
    let (child, _) = start(&["--in-memory", "--users", "off"], &[("GRAPHBUFF_DATA", &empty)]);
    stop(child);
    assert!(!empty.exists(), "--in-memory created {}", empty.display());

    for dir in [data, logs] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn in_memory_and_data_exclude_each_other() {
    let out = Command::new(BIN).args(["--in-memory", "--data", "x"]).env("GRAPHBUFF_CONFIG", "-").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--in-memory and --data exclude each other"));
}

#[test]
fn config_file_sets_data_and_logs_and_is_created_when_missing() {
    let dir = temp_dir("config");
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("graphbuff.conf");
    std::fs::write(&conf, "data = db   # relative to the file\nlog-dir = mylogs\nlog-level = warn\nusers = off\n").unwrap();
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0", "--config"]).arg(&conf);
    for key in ["GRAPHBUFF_TOKEN", "GRAPHBUFF_USERS", "GRAPHBUFF_DATA", "GRAPHBUFF_LOG_DIR", "GRAPHBUFF_LOG_LEVEL", "GRAPHBUFF_CONFIG"] {
        cmd.env_remove(key);
    }
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    stop(child);
    assert!(line.contains(&format!("data in {}", dir.join("db").display())), "{line}");
    assert!(dir.join("db").join("catalog").exists());
    assert!(dir.join("mylogs").join("graphbuff.log").exists());

    // A missing config file is created with every setting commented out.
    let fresh = dir.join("new").join("graphbuff.conf");
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0", "--in-memory", "--users", "off"]).env("GRAPHBUFF_CONFIG", &fresh).env_remove("GRAPHBUFF_USERS");
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    stop(child);
    let text = std::fs::read_to_string(&fresh).unwrap();
    assert!(text.contains("# bind = 127.0.0.1:7475"), "{text}");

    let bad = dir.join("bad.conf");
    std::fs::write(&bad, "nope = 1\n").unwrap();
    let out = Command::new(BIN).arg("--config").arg(&bad).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown setting `nope`"));
    let _ = std::fs::remove_dir_all(&dir);
}
