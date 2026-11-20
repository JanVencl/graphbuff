//! The per-query memory limit over HTTP (`ServerConfig::query_max_memory`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use graphbuff::DatabaseManager;
use graphbuff::accounting::CountingAllocator;
use graphbuff_server::{Server, ServerConfig};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut chunked = false;
    loop {
        let mut h = String::new();
        r.read_line(&mut h).unwrap();
        if h.trim_end().is_empty() {
            break;
        }
        chunked |= h.to_ascii_lowercase().starts_with("transfer-encoding: chunked");
    }
    let mut body = String::new();
    if !chunked {
        r.read_to_string(&mut body).unwrap();
        return (status, body);
    }
    loop {
        let mut size = String::new();
        r.read_line(&mut size).unwrap();
        let n = usize::from_str_radix(size.trim(), 16).unwrap();
        let mut chunk = vec![0; n + 2];
        r.read_exact(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        body.push_str(std::str::from_utf8(&chunk[..n]).unwrap());
    }
    (status, body)
}

#[test]
fn queries_over_the_limit_answer_413() {
    let server = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig { bind: "127.0.0.1:0".into(), query_max_memory: Some(32 << 20), ..ServerConfig::default() },
    )
    .unwrap();
    let addr = server.local_addr();
    let (status, body) = post(addr, "/query", r#"{"query": "UNWIND range(1, 3000000) AS i RETURN collect(i)"}"#);
    assert_eq!(status, 413, "{body}");
    assert!(body.contains(r#""code":"MemoryLimit""#), "{body}");
    let (status, body) = post(addr, "/query", r#"{"query": "UNWIND range(1, 1000) AS i RETURN sum(i)"}"#);
    assert_eq!(status, 200, "{body}");
    // In an explicit transaction too: the statement fails and the transaction is rolled back.
    let (status, body) = post(addr, "/tx", "");
    assert_eq!(status, 201, "{body}");
    let path = body.split(r#""path":""#).nth(1).unwrap().split('"').next().unwrap().to_string();
    let (status, body) = post(addr, &path, r#"{"query": "UNWIND range(1, 3000000) AS i RETURN collect(i)"}"#);
    assert_eq!(status, 413, "{body}");
    assert!(body.contains(r#""rolledBack":true"#), "{body}");
}

#[test]
fn large_results_stream_in_chunks() {
    let server = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig { bind: "127.0.0.1:0".into(), query_max_memory: Some(16 << 20), ..ServerConfig::default() },
    )
    .unwrap();
    let addr = server.local_addr();
    // Collected (in a transaction), the rows are over the limit; streamed, they pass.
    let q = r#"{"query": "UNWIND range(1, 400) AS i UNWIND range(1, 1000) AS j RETURN i, toString(j) AS s"}"#;
    let (status, body) = post(addr, "/tx", "");
    assert_eq!(status, 201, "{body}");
    let path = body.split(r#""path":""#).nth(1).unwrap().split('"').next().unwrap().to_string();
    let (status, body) = post(addr, &path, q);
    assert_eq!(status, 413, "{body}");
    let (status, body) = post(addr, "/query", q);
    assert_eq!(status, 200, "{}", &body[..200.min(body.len())]);
    assert!(body.starts_with(r#"{"columns":["i","s"],"rows":[[1,"1"],"#), "{}", &body[..100]);
    assert!(body.contains(r#"[400,"1000"]],"stats":{"#), "{}", &body[body.len() - 300..]);
    assert!(body.ends_with(r#""containsUpdates":false}}"#), "{}", &body[body.len() - 100..]);
    assert_eq!(body.matches("],[").count() + 1, 400000);

    // A failure after rows went out ends the object with the error.
    let (status, body) = post(addr, "/query", r#"{"query": "UNWIND range(1, 100000) AS i RETURN 10 / (99000 - i) AS x"}"#);
    assert_eq!(status, 200);
    let tail = r#"[0]],"error":{"code":"RuntimeError","message":"division by zero"}}"#;
    assert!(body.ends_with(tail), "{}", &body[body.len() - 100..]);
    assert!(!body.contains(r#""stats""#));
    // Before that, it is answered as usual.
    let (status, body) = post(addr, "/query", r#"{"query": "UNWIND range(1, 10) AS i RETURN 10 / (5 - i) AS x"}"#);
    assert_eq!(status, 400, "{body}");
    assert!(body.starts_with(r#"{"error":"#), "{body}");
}
