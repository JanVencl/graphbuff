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
    loop {
        let mut h = String::new();
        r.read_line(&mut h).unwrap();
        if h.trim_end().is_empty() {
            break;
        }
    }
    let mut body = String::new();
    r.read_to_string(&mut body).unwrap();
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
