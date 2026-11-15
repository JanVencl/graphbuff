//! The real binary stops cleanly on SIGTERM (Unix) or Ctrl-Break (Windows): it exits 0
//! and leaves durable databases checkpointed, with nothing left in their logs to replay.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_graphbuff-server");

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphbuff-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Sends a query; returns the status line.
fn query(addr: &str, body: &str) -> String {
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
    reply.lines().next().unwrap_or_default().to_string()
}

/// Asks the server to stop the way a service manager or a console does.
#[cfg(unix)]
fn request_stop(child: &Child) {
    let ok = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(ok.success());
}

#[cfg(windows)]
fn request_stop(child: &Child) {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, process_group: u32) -> i32;
    }
    const CTRL_BREAK_EVENT: u32 = 1;
    // The child leads its own process group, so only it gets the event.
    assert_ne!(unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) }, 0);
}

fn spawn(cmd: &mut Command) -> Child {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap()
}

#[test]
fn stop_request_checkpoints_and_exits_zero() {
    let data = temp_dir("shutdown");
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0", "--data"]).arg(&data);
    for key in ["GRAPHBUFF_TOKEN", "GRAPHBUFF_DATA", "GRAPHBUFF_LOG_DIR", "GRAPHBUFF_LOG_LEVEL"] {
        cmd.env_remove(key);
    }
    let mut child = spawn(&mut cmd);
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut listening = String::new();
    out.read_line(&mut listening).unwrap();
    let addr = listening
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no address in {listening:?}"))
        .to_string();

    let ok = query(&addr, r#"{"query": "UNWIND range(1, 100) AS i CREATE (:N {i: i})"}"#);
    let wal = data.join("databases/graphbuff/graph.wal");
    let logged = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);

    request_stop(&child);
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("the server did not stop");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();

    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(status.success(), "{status:?}");
    assert!(rest.contains("stopping") && rest.contains("stopped in "), "{rest}");
    // The commit was logged; the checkpoint moved it into the snapshot.
    let after = std::fs::metadata(&wal).unwrap().len();
    assert!(logged > after, "log {logged} bytes before the stop, {after} after");
    assert!(data.join("databases/graphbuff/graph.snapshot").exists());
    let events = std::fs::read_to_string(data.join("logs/graphbuff.log")).unwrap();
    assert!(events.contains(" INFO  stop requested; stopping"), "{events}");
    assert!(events.contains(" INFO  stopped in "), "{events}");

    // The data is there on the next start.
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0", "--data"]).arg(&data);
    cmd.env_remove("GRAPHBUFF_TOKEN");
    let mut child = spawn(&mut cmd);
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let addr = line.split("http://").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().to_string();
    let mut conn = TcpStream::connect(&addr).unwrap();
    let body = r#"{"query": "MATCH (n:N) RETURN count(n) AS c"}"#;
    write!(
        conn,
        "POST /query HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(reply.contains(r#""rows":[[100]]"#), "{reply}");
    let _ = std::fs::remove_dir_all(&data);
}
