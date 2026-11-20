//! End-to-end tests: a real server on a local port, spoken to over raw TCP.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use graphbuff::{DatabaseManager, StorageKind};
use graphbuff_server::json::{self, Json};
use graphbuff_server::{Server, ServerConfig};

fn start(config: ServerConfig) -> Server {
    let mgr = Arc::new(DatabaseManager::in_memory());
    Server::start(
        mgr,
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            ..config
        },
    )
    .unwrap()
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn json(&self) -> Json {
        json::parse(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn error_code(&self) -> String {
        match &self.json() {
            Json::Object(m) => match &m["error"] {
                Json::Object(e) => match &e["code"] {
                    Json::String(c) => c.clone(),
                    _ => panic!(),
                },
                _ => panic!(),
            },
            _ => panic!(),
        }
    }
}

/// One keep-alive connection.
struct Client {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let w = TcpStream::connect(addr).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Client {
            r: BufReader::new(w.try_clone().unwrap()),
            w,
        }
    }

    fn send_raw(&mut self, raw: &[u8]) {
        self.w.write_all(raw).unwrap();
    }

    fn read_reply(&mut self) -> Reply {
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let status: u16 = line
            .split(' ')
            .nth(1)
            .unwrap_or_else(|| panic!("bad status line {line:?}"))
            .parse()
            .unwrap();
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            self.r.read_line(&mut h).unwrap();
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            let (n, v) = h.split_once(':').unwrap();
            headers.push((n.to_string(), v.trim().to_string()));
        }
        let len: usize = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.parse().unwrap())
            .unwrap_or(0);
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).unwrap();
        Reply {
            status,
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    }

    fn request(&mut self, method: &str, path: &str, body: Option<&str>, extra: &[(&str, &str)]) -> Reply {
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: test\r\n");
        for (n, v) in extra {
            req.push_str(&format!("{n}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        }
        req.push_str("\r\n");
        req.push_str(body.unwrap_or(""));
        self.send_raw(req.as_bytes());
        self.read_reply()
    }

    fn query(&mut self, path: &str, query: &str, params: &str) -> Reply {
        let mut body = String::from(r#"{"query":"#);
        json::write_str(&mut body, query);
        body.push_str(r#","params":"#);
        body.push_str(params);
        body.push('}');
        self.request("POST", path, Some(&body), &[])
    }

    /// Runs a query that must succeed; returns its rows.
    fn rows(&mut self, path: &str, query: &str, params: &str) -> Vec<Json> {
        let r = self.query(path, query, params);
        assert_eq!(r.status, 200, "{query}: {}", r.body);
        match r.json() {
            Json::Object(mut m) => match m.remove("rows") {
                Some(Json::Array(rows)) => rows,
                _ => panic!(),
            },
            _ => panic!(),
        }
    }
}

fn obj(pairs: &[(&str, Json)]) -> Json {
    Json::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn s(v: &str) -> Json {
    Json::String(v.into())
}

#[test]
fn health_routes_and_methods() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    let r = c.request("GET", "/health", None, &[]);
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"status":"ok"}"#));
    assert_eq!(r.header("content-type"), Some("application/json; charset=utf-8"));

    let r = c.request("GET", "/nope", None, &[]);
    assert_eq!((r.status, r.error_code().as_str()), (404, "NotFound"));
    let r = c.request("GET", "/query", None, &[]);
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("POST"));
    let r = c.request("GET", "/db?x=1", None, &[]);
    assert_eq!(r.status, 200);
}

#[test]
fn queries_parameters_and_value_encoding() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    let r = c.query(
        "/query",
        "CREATE (a:Person {name: $name, age: $age, score: $score, tags: $tags})-[k:KNOWS {since: 2020}]->(b:Person {name: 'Bob'}) RETURN a.name AS name",
        r#"{"name": "Ann \"A\" é😀", "age": 31, "score": 2.0, "tags": ["x", "y"]}"#,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    assert_eq!(m["columns"], Json::Array(vec![s("name")]));
    assert_eq!(m["rows"], Json::Array(vec![Json::Array(vec![s("Ann \"A\" é😀")])]));
    let Json::Object(stats) = &m["stats"] else { panic!() };
    assert_eq!(stats["nodesCreated"], Json::Int(2));
    assert_eq!(stats["relationshipsCreated"], Json::Int(1));
    assert_eq!(stats["containsUpdates"], Json::Bool(true));

    // Integers stay integers and floats stay floats in both directions.
    let rows = c.rows(
        "/query",
        "MATCH (a {age: $age}) RETURN a.age, a.score, a.tags, 1.5 * 2, null, {k: [1, 'two']}, 0.0 / 0.0",
        r#"{"age": 31}"#,
    );
    assert_eq!(
        rows,
        vec![Json::Array(vec![
            Json::Int(31),
            Json::Float(2.0),
            Json::Array(vec![s("x"), s("y")]),
            Json::Float(3.0),
            Json::Null,
            obj(&[("k", Json::Array(vec![Json::Int(1), s("two")]))]),
            s("NaN"),
        ])]
    );

    // Nodes, relationships and paths carry a `$type`.
    let rows = c.rows(
        "/db/graphbuff/query",
        "MATCH p = (a)-[k:KNOWS]->(b) RETURN b, k, p",
        "null",
    );
    let Json::Array(row) = &rows[0] else { panic!() };
    let bob = obj(&[
        ("$type", s("node")),
        ("id", Json::Int(1)),
        ("labels", Json::Array(vec![s("Person")])),
        ("properties", obj(&[("name", s("Bob"))])),
    ]);
    let knows = obj(&[
        ("$type", s("relationship")),
        ("id", Json::Int(0)),
        ("type", s("KNOWS")),
        ("start", Json::Int(0)),
        ("end", Json::Int(1)),
        ("properties", obj(&[("since", Json::Int(2020))])),
    ]);
    assert_eq!(row[0], bob);
    assert_eq!(row[1], knows);
    let Json::Object(path) = &row[2] else { panic!() };
    assert_eq!(path["$type"], s("path"));
    assert_eq!(path["relationships"], Json::Array(vec![knows]));
    let Json::Array(nodes) = &path["nodes"] else { panic!() };
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[1], bob);
}

#[test]
fn explain_and_profile_return_the_plan() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    let r = c.query("/query", "UNWIND [1, 2, 3] AS x RETURN x", "null");
    let Json::Object(m) = r.json() else { panic!() };
    assert!(!m.contains_key("plan"));

    let r = c.query("/query", "PROFILE UNWIND [1, 2, 3] AS x RETURN x LIMIT 2", "null");
    assert_eq!(r.status, 200, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    assert_eq!(m["rows"], Json::Array(vec![Json::Array(vec![Json::Int(1)]), Json::Array(vec![Json::Int(2)])]));
    let Json::Object(plan) = &m["plan"] else { panic!("{}", r.body) };
    assert_eq!(plan["profiled"], Json::Bool(true));
    let Json::Array(lines) = &plan["lines"] else { panic!() };
    let Json::Object(first) = &lines[0] else { panic!() };
    assert_eq!(first["depth"], Json::Int(0));
    assert_eq!(first["operator"], s("Select"));
    assert_eq!(first["rows"], Json::Int(2));
    assert_eq!(first["estimatedRows"], Json::Null);
    assert!(matches!(first["timeMs"], Json::Float(_) | Json::Int(_)));

    let r = c.query("/query", "EXPLAIN MATCH (n) RETURN n", "null");
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Object(plan) = &m["plan"] else { panic!() };
    assert_eq!(plan["profiled"], Json::Bool(false));
    let Json::Array(lines) = &plan["lines"] else { panic!() };
    let Json::Object(scan) = &lines[2] else { panic!() };
    assert_eq!(scan["operator"], s("AllNodesScan (n)"));
    assert!(matches!(scan["estimatedRows"], Json::Float(_)));
    assert_eq!(scan["rows"], Json::Null);
    assert_eq!(scan["timeMs"], Json::Null);
}

#[test]
fn errors_map_to_status_codes() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    c.rows("/query", "CREATE (:A)-[:R]->(:B)", "{}");

    let r = c.query("/query", "MATCH (n RETURN n", "{}");
    assert_eq!((r.status, r.error_code().as_str()), (400, "SyntaxError"));
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Object(e) = &m["error"] else { panic!() };
    assert!(matches!(e["offset"], Json::Int(_)));

    let cases: &[(&str, Option<&str>, u16, &str)] = &[
        ("/query", Some(r#"{"query": "MATCH (a:A) DELETE a"}"#), 409, "ConstraintError"),
        ("/query", Some(r#"{"query": "RETURN 1 / 0"}"#), 400, "RuntimeError"),
        ("/query", Some(r#"{"query": "RETURN $missing"}"#), 400, "SemanticError"),
        ("/db/nosuchdb/query", Some(r#"{"query": "RETURN 1"}"#), 404, "DatabaseNotFound"),
        ("/db/1bad/query", Some(r#"{"query": "RETURN 1"}"#), 400, "SemanticError"),
        ("/query", Some("{not json"), 400, "BadRequest"),
        ("/query", Some(r#"{"params": {}}"#), 400, "BadRequest"),
        ("/query", Some(r#"{"query": 5}"#), 400, "BadRequest"),
        ("/query", Some(r#"{"query": "RETURN 1", "params": [1]}"#), 400, "BadRequest"),
        ("/query", Some(r#"{"query": "RETURN 1", "parameters": {}}"#), 400, "BadRequest"),
        ("/query", Some("[]"), 400, "BadRequest"),
    ];
    for (path, body, status, code) in cases {
        let r = c.request("POST", path, *body, &[]);
        assert_eq!((r.status, r.error_code().as_str()), (*status, *code), "{body:?}: {}", r.body);
    }
    // A failed write left nothing behind, and the connection is still usable.
    let rows = c.rows("/query", "MATCH (n) RETURN count(n)", "{}");
    assert_eq!(rows, vec![Json::Array(vec![Json::Int(2)])]);
}

#[test]
fn wrong_content_type_is_rejected() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    c.send_raw(b"POST /query HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}");
    assert_eq!(c.read_reply().status, 415);
}

#[test]
fn database_commands_and_listing() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    c.rows("/query", "CREATE DATABASE movies", "{}");
    c.rows("/db/movies/query", "CREATE (:Movie {title: $t})", r#"{"t": "Heat"}"#);
    // Case-insensitive names, and percent-encoded paths.
    let rows = c.rows("/db/MOVIES/query", "MATCH (m:Movie) RETURN m.title", "{}");
    assert_eq!(rows, vec![Json::Array(vec![s("Heat")])]);
    let rows = c.rows("/db/mov%69es/query", "MATCH (m) RETURN count(m)", "{}");
    assert_eq!(rows, vec![Json::Array(vec![Json::Int(1)])]);
    // The default database is untouched; USE inside a query reaches another one.
    assert_eq!(
        c.rows("/query", "MATCH (n) RETURN count(n)", "{}"),
        vec![Json::Array(vec![Json::Int(0)])]
    );
    assert_eq!(
        c.rows("/query", "USE movies MATCH (n) RETURN count(n)", "{}"),
        vec![Json::Array(vec![Json::Int(1)])]
    );

    let r = c.request("GET", "/db", None, &[]);
    assert_eq!(r.status, 200);
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Array(dbs) = &m["databases"] else { panic!() };
    assert_eq!(dbs.len(), 2);
    assert_eq!(
        dbs[1],
        obj(&[
            ("name", s("movies")),
            ("storage", s("memory")),
            ("default", Json::Bool(false)),
            ("nodes", Json::Int(1)),
            ("relationships", Json::Int(0)),
        ])
    );
    let rows = c.rows("/query", "SHOW DATABASES", "{}");
    assert_eq!(rows.len(), 2);
}

#[test]
fn keep_alive_chunked_and_expect_continue() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    for i in 0..20 {
        c.rows("/query", "CREATE (:N {i: $i})", &format!(r#"{{"i": {i}}}"#));
    }
    // Chunked body with a chunk extension and a trailer, after Expect: 100-continue.
    let body = r#"{"query": "MATCH (n:N) RETURN count(n), sum(n.i)"}"#;
    let (a, b) = body.split_at(10);
    c.send_raw(
        b"POST /query HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n",
    );
    let mut cont = String::new();
    c.r.read_line(&mut cont).unwrap();
    assert!(cont.starts_with("HTTP/1.1 100"), "{cont:?}");
    let mut blank = String::new();
    c.r.read_line(&mut blank).unwrap();
    c.send_raw(
        format!(
            "{:x};ext=1\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\nX-Trailer: y\r\n\r\n",
            a.len(),
            b.len()
        )
        .as_bytes(),
    );
    let r = c.read_reply();
    assert_eq!(r.status, 200, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    assert_eq!(
        m["rows"],
        Json::Array(vec![Json::Array(vec![Json::Int(20), Json::Int(190)])])
    );

    // Connection: close is honored.
    let r = c.request("GET", "/health", None, &[("Connection", "close")]);
    assert_eq!(r.header("connection"), Some("close"));
    let mut rest = Vec::new();
    assert_eq!(c.r.read_to_end(&mut rest).unwrap(), 0);
}

#[test]
fn oversized_and_malformed_requests() {
    let server = start(ServerConfig {
        max_body_bytes: 1000,
        ..ServerConfig::default()
    });
    let mut c = Client::connect(server.local_addr());
    let big = format!(r#"{{"query": "RETURN '{}'"}}"#, "x".repeat(2000));
    let r = c.request("POST", "/query", Some(&big), &[]);
    assert_eq!(r.status, 413);

    let mut c = Client::connect(server.local_addr());
    c.send_raw(b"GARBAGE\r\n\r\n");
    assert_eq!(c.read_reply().status, 400);

    let mut c = Client::connect(server.local_addr());
    c.send_raw(b"GET /health HTTP/2.0\r\n\r\n");
    assert_eq!(c.read_reply().status, 505);
}

#[test]
fn bearer_token() {
    let server = start(ServerConfig {
        token: Some("s3cret".into()),
        ..ServerConfig::default()
    });
    let mut c = Client::connect(server.local_addr());
    assert_eq!(c.request("GET", "/health", None, &[]).status, 200);
    let r = c.request("GET", "/db", None, &[]);
    assert_eq!(r.status, 401);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    let r = c.request("GET", "/db", None, &[("Authorization", "Bearer wrong!")]);
    assert_eq!(r.status, 401);
    let r = c.request("GET", "/db", None, &[("Authorization", "Bearer s3cret")]);
    assert_eq!(r.status, 200);
}

#[test]
fn concurrent_clients() {
    let server = start(ServerConfig::default());
    let addr = server.local_addr();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            std::thread::spawn(move || {
                let mut c = Client::connect(addr);
                for i in 0..25 {
                    c.rows(
                        "/query",
                        "CREATE (:W {t: $t, i: $i})",
                        &format!(r#"{{"t": {t}, "i": {i}}}"#),
                    );
                    c.rows("/query", "MATCH (w:W) RETURN count(w)", "{}");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut c = Client::connect(addr);
    assert_eq!(
        c.rows("/query", "MATCH (w:W) RETURN count(w)", "{}"),
        vec![Json::Array(vec![Json::Int(200)])]
    );
}

#[test]
fn oversized_transaction_is_413() {
    let dir: PathBuf = std::env::temp_dir().join(format!("graphbuff-server-txcap-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mgr = Arc::new(DatabaseManager::open_with(&dir, StorageKind::persistent()).unwrap());
    let server = Server::start(
        mgr,
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut c = Client::connect(server.local_addr());
    c.rows(
        "/query",
        "CREATE DATABASE small OPTIONS {storage: 'persistent', maxTransactionBytes: 65536}",
        "{}",
    );
    let big = "UNWIND range(1, 2000) AS i CREATE (:N {i: i, s: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'})";
    let r = c.query("/db/small/query", big, "{}");
    assert_eq!((r.status, r.error_code().as_str()), (413, "TransactionTooLarge"), "{}", r.body);
    // In an explicit transaction it ends the transaction.
    let tx = begin(&mut c, "/db/small/tx");
    let r = c.query(&tx, big, "{}");
    assert_eq!(r.status, 413);
    assert!(rolled_back(&r));
    assert_eq!(c.query(&tx, "RETURN 1", "{}").status, 404);
    assert_eq!(count(&mut c, "/db/small/query", "N"), 0);
    drop(c);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shutdown_is_prompt_and_data_is_durable() {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "graphbuff-server-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let open = || {
        Arc::new(DatabaseManager::open_with(&dir, StorageKind::persistent()).unwrap())
    };
    let config = || ServerConfig {
        bind: "127.0.0.1:0".into(),
        ..ServerConfig::default()
    };
    {
        let mgr = open();
        let server = Server::start(Arc::clone(&mgr), config()).unwrap();
        let mut c = Client::connect(server.local_addr());
        c.rows("/query", "CREATE (:Kept {v: 1})", "{}");
        // An idle keep-alive connection does not hold up shutdown.
        let started = Instant::now();
        server.shutdown();
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(c);
        assert_eq!(Arc::strong_count(&mgr), 1, "server threads released the manager");
    }
    let server = Server::start(open(), config()).unwrap();
    let mut c = Client::connect(server.local_addr());
    assert_eq!(
        c.rows("/query", "MATCH (k:Kept) RETURN k.v", "{}"),
        vec![Json::Array(vec![Json::Int(1)])]
    );
    drop(c);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- explicit transactions ----------------------------------------------------------------

/// Begins a transaction at `path` (`/tx` or `/db/{name}/tx`); returns the path to send
/// its statements to.
fn begin(c: &mut Client, path: &str) -> String {
    let r = c.request("POST", path, None, &[]);
    assert_eq!(r.status, 201, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Object(t) = &m["transaction"] else { panic!() };
    let Json::String(p) = &t["path"] else { panic!() };
    assert_eq!(r.header("location"), Some(p.as_str()));
    let Json::String(id) = &t["id"] else { panic!() };
    assert_eq!(id.len(), 32);
    assert!(p.ends_with(id.as_str()));
    p.clone()
}

fn count(c: &mut Client, path: &str, label: &str) -> i64 {
    match c.rows(path, &format!("MATCH (n:{label}) RETURN count(n)"), "{}").as_slice() {
        [Json::Array(row)] => match row.as_slice() {
            [Json::Int(n)] => *n,
            _ => panic!(),
        },
        _ => panic!(),
    }
}

fn rolled_back(r: &Reply) -> bool {
    match r.json() {
        Json::Object(m) => m.get("rolledBack") == Some(&Json::Bool(true)),
        _ => false,
    }
}

#[test]
fn transaction_commit_spans_requests_and_connections() {
    let server = start(ServerConfig::default());
    let addr = server.local_addr();
    let mut a = Client::connect(addr);
    let mut b = Client::connect(addr);
    let tx = begin(&mut a, "/tx");
    assert!(tx.starts_with("/db/graphbuff/tx/"), "{tx}");

    a.rows(&tx, "CREATE (:T {v: $v})", r#"{"v": 1}"#);
    // Another connection may send the next statement; it sees the first one's changes.
    let rows = b.rows(&tx, "CREATE (:T {v: 2}) WITH 1 AS x MATCH (t:T) RETURN count(t)", "{}");
    assert_eq!(rows, vec![Json::Array(vec![Json::Int(2)])]);
    // Outside the transaction nothing is visible yet, and reads do not wait.
    assert_eq!(count(&mut b, "/query", "T"), 0);

    let r = a.request("POST", &format!("{tx}/commit"), None, &[]);
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"committed":true}"#));
    assert_eq!(count(&mut b, "/query", "T"), 2);

    // The transaction is gone.
    let r = a.query(&tx, "RETURN 1", "{}");
    assert_eq!((r.status, r.error_code().as_str()), (404, "TransactionNotFound"));
    let r = a.request("POST", &format!("{tx}/commit"), None, &[]);
    assert_eq!(r.status, 404);

    // A named database, through the `/db/{name}` routes.
    a.rows("/query", "CREATE DATABASE other", "{}");
    let tx = begin(&mut a, "/db/OTHER/tx");
    assert!(tx.starts_with("/db/other/tx/"), "{tx}");
    a.rows(&tx, "CREATE (:T)", "{}");
    // The id does not work against another database.
    let id = tx.rsplit('/').next().unwrap();
    let r = a.query(&format!("/tx/{id}"), "RETURN 1", "{}");
    assert_eq!(r.status, 404);
    // USE of the transaction's own database is fine.
    a.rows(&tx, "USE other CREATE (:T)", "{}");
    let r = a.request("POST", &format!("/db/other/tx/{id}/commit"), Some("{}"), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(count(&mut a, "/db/other/query", "T"), 2);
    assert_eq!(count(&mut a, "/query", "T"), 2);

    let r = a.request("POST", "/db/nosuchdb/tx", None, &[]);
    assert_eq!((r.status, r.error_code().as_str()), (404, "DatabaseNotFound"));
    let r = a.request("POST", "/tx", Some(r#"{"query": "RETURN 1"}"#), &[]);
    assert_eq!(r.status, 400);
    let r = a.request("GET", "/tx", None, &[]);
    assert_eq!(r.status, 405);
    let r = a.request("PUT", &tx, None, &[]);
    assert_eq!((r.status, r.header("allow")), (405, Some("POST, DELETE")));
}

#[test]
fn transaction_rollback_and_failures() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());

    // DELETE rolls back.
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T)", "{}");
    let r = c.request("DELETE", &tx, None, &[]);
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"rolledBack":true}"#));
    let r = c.request("DELETE", &tx, None, &[]);
    assert_eq!((r.status, r.error_code().as_str()), (404, "TransactionNotFound"));
    assert_eq!(count(&mut c, "/query", "T"), 0);

    // Rejected statements leave the transaction open.
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T)", "{}");
    for (body, status, code) in [
        (r#"{"query": "MATCH (n RETURN n"}"#, 400, "SyntaxError"),
        (r#"{"query": "CREATE DATABASE x"}"#, 400, "Unsupported"),
        (r#"{"query": "USE system"}"#, 400, "Unsupported"),
        (r#"{"query": "USE other RETURN 1"}"#, 400, "SemanticError"),
        (r#"{"query": "USE $d RETURN 1", "params": {"d": "x"}}"#, 400, "SemanticError"),
        (r#"{"params": {}}"#, 400, "BadRequest"),
    ] {
        let r = c.request("POST", &tx, Some(body), &[]);
        assert_eq!((r.status, r.error_code().as_str()), (status, code), "{body}: {}", r.body);
        assert!(!rolled_back(&r), "{body}");
    }
    assert_eq!(count(&mut c, &tx, "T"), 1);

    // A statement that fails rolls the whole transaction back.
    let r = c.query(&tx, "RETURN 1 / 0", "{}");
    assert_eq!((r.status, r.error_code().as_str()), (400, "RuntimeError"));
    assert!(rolled_back(&r), "{}", r.body);
    let r = c.query(&tx, "RETURN 1", "{}");
    assert_eq!(r.status, 404);
    assert_eq!(count(&mut c, "/query", "T"), 0);

    // Also a constraint violation, checked when its statement ends.
    c.rows("/query", "CREATE CONSTRAINT FOR (u:U) REQUIRE u.id IS UNIQUE", "{}");
    c.rows("/query", "CREATE (:U {id: 1})", "{}");
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T)", "{}");
    let r = c.query(&tx, "CREATE (:U {id: 1})", "{}");
    assert_eq!((r.status, r.error_code().as_str()), (409, "ConstraintError"));
    assert!(rolled_back(&r));
    assert_eq!(count(&mut c, "/query", "T"), 0);

    // The database accepts writers again.
    c.rows("/query", "CREATE (:T)", "{}");
    assert_eq!(count(&mut c, "/query", "T"), 1);
}

#[test]
fn writers_wait_for_an_open_transaction() {
    let server = start(ServerConfig::default());
    let addr = server.local_addr();
    let mut c = Client::connect(addr);
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T {by: 'tx'})", "{}");

    let writer = std::thread::spawn(move || {
        let mut w = Client::connect(addr);
        let rows = w.rows("/query", "CREATE (:T {by: 'query'}) WITH 1 AS x MATCH (t:T) RETURN count(t)", "{}");
        let tx2 = begin(&mut w, "/tx");
        w.rows(&tx2, "CREATE (:T {by: 'tx2'})", "{}");
        let r = w.request("POST", &format!("{tx2}/commit"), None, &[]);
        assert_eq!(r.status, 200);
        rows
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!writer.is_finished(), "a writing query ran while the transaction was open");
    // Reads keep working meanwhile.
    assert_eq!(count(&mut c, "/query", "T"), 0);
    let r = c.request("POST", &format!("{tx}/commit"), None, &[]);
    assert_eq!(r.status, 200);
    // The query ran after the commit, so it saw the transaction's node.
    assert_eq!(writer.join().unwrap(), vec![Json::Array(vec![Json::Int(2)])]);
    assert_eq!(count(&mut c, "/query", "T"), 3);
}

#[test]
fn abandoned_transactions_time_out() {
    let server = start(ServerConfig {
        transaction_timeout: Duration::from_millis(300),
        ..ServerConfig::default()
    });
    let mut c = Client::connect(server.local_addr());
    let r = c.request("POST", "/tx", Some("{}"), &[]);
    assert_eq!(r.status, 201, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Object(t) = &m["transaction"] else { panic!() };
    assert_eq!(t["timeoutMs"], Json::Int(300));
    let Json::String(tx) = &t["path"] else { panic!() };

    // Each request restarts the timeout.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        c.rows(tx, "CREATE (:T)", "{}");
    }
    let started = Instant::now();
    // A writer waits for the abandoned transaction to time out, then runs.
    c.rows("/query", "CREATE (:Q)", "{}");
    assert!(started.elapsed() >= Duration::from_millis(100), "{:?}", started.elapsed());
    let r = c.query(tx, "RETURN 1", "{}");
    assert_eq!((r.status, r.error_code().as_str()), (404, "TransactionNotFound"));
    assert_eq!(count(&mut c, "/query", "T"), 0);
    assert_eq!(count(&mut c, "/query", "Q"), 1);
}

#[test]
fn shutdown_rolls_back_open_transactions() {
    let mgr = Arc::new(DatabaseManager::in_memory());
    let server = Server::start(
        Arc::clone(&mgr),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let addr = server.local_addr();
    let mut c = Client::connect(addr);
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T)", "{}");
    // A second client waits to begin one.
    let waiting = std::thread::spawn(move || {
        let mut w = Client::connect(addr);
        w.request("POST", "/tx", None, &[]).status
    });
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    server.shutdown();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(waiting.join().unwrap(), 503);
    let db = mgr.database("graphbuff").unwrap();
    assert_eq!(db.node_count(), 0);
    // Nothing holds the database any more.
    db.execute("CREATE (:After)").unwrap();
    drop(c);
}

// ---- query timeouts and cancellation ------------------------------------------------------

/// A billion rows: never finishes in a test.
const SLOW: &str = "MATCH (a:N), (b:N), (c:N) RETURN count(*)";

fn slow_body(timeout_ms: Option<u64>) -> String {
    let mut body = String::from(r#"{"query":"#);
    json::write_str(&mut body, SLOW);
    if let Some(ms) = timeout_ms {
        body.push_str(&format!(r#","timeoutMs":{ms}"#));
    }
    body.push('}');
    body
}

#[test]
fn queries_time_out() {
    let server = start(ServerConfig {
        query_timeout: Some(Duration::from_millis(300)),
        ..ServerConfig::default()
    });
    let mut c = Client::connect(server.local_addr());
    c.rows("/query", "UNWIND range(1, 1000) AS i CREATE (:N)", "{}");

    // The server's limit.
    let started = Instant::now();
    let r = c.request("POST", "/query", Some(&slow_body(None)), &[]);
    assert_eq!((r.status, r.error_code().as_str()), (408, "Timeout"), "{}", r.body);
    assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());
    // A request may ask for less, but not for more.
    let started = Instant::now();
    let r = c.request("POST", "/query", Some(&slow_body(Some(50))), &[]);
    assert_eq!(r.status, 408, "{}", r.body);
    assert!(r.body.contains("50 ms"), "{}", r.body);
    assert!(started.elapsed() < Duration::from_millis(300), "{:?}", started.elapsed());
    let r = c.request("POST", "/db/graphbuff/query", Some(&slow_body(Some(60_000))), &[]);
    assert!(r.body.contains("300 ms"), "{}", r.body);
    // A writing query that times out is rolled back.
    let r = c.request(
        "POST",
        "/query",
        Some(r#"{"query": "MATCH (a:N), (b:N), (c:N) CREATE (:X)", "timeoutMs": 50}"#),
        &[],
    );
    assert_eq!(r.status, 408, "{}", r.body);
    assert_eq!(count(&mut c, "/query", "X"), 0);

    for bad in [r#""5""#, "0", "-1", "1.5"] {
        let body = format!(r#"{{"query": "RETURN 1", "timeoutMs": {bad}}}"#);
        let r = c.request("POST", "/query", Some(&body), &[]);
        assert_eq!((r.status, r.error_code().as_str()), (400, "BadRequest"), "{bad}: {}", r.body);
    }
    let r = c.request("POST", "/query", Some(r#"{"query": "RETURN 1", "timeoutMs": null}"#), &[]);
    assert_eq!(r.status, 200, "{}", r.body);

    // In a transaction, a statement that times out rolls it back.
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T)", "{}");
    let r = c.request("POST", &tx, Some(&slow_body(Some(50))), &[]);
    assert_eq!((r.status, r.error_code().as_str()), (408, "Timeout"), "{}", r.body);
    assert!(rolled_back(&r), "{}", r.body);
    assert_eq!(c.query(&tx, "RETURN 1", "{}").status, 404);
    assert_eq!(count(&mut c, "/query", "T"), 0);

    // Waiting to begin (or to write) behind an open transaction is limited too.
    let tx = begin(&mut c, "/tx");
    let started = Instant::now();
    let r = c.request("POST", "/tx", None, &[]);
    assert_eq!((r.status, r.error_code().as_str()), (408, "Timeout"), "{}", r.body);
    let r = c.request("POST", "/query", Some(r#"{"query": "CREATE (:W)", "timeoutMs": 50}"#), &[]);
    assert_eq!(r.status, 408, "{}", r.body);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let r = c.request("DELETE", &tx, None, &[]);
    assert_eq!(r.status, 200);
    c.rows("/query", "CREATE (:W)", "{}");
}

#[test]
fn rollback_cancels_the_running_statement() {
    let server = start(ServerConfig::default());
    let addr = server.local_addr();
    let mut c = Client::connect(addr);
    c.rows("/query", "UNWIND range(1, 1000) AS i CREATE (:N)", "{}");
    let tx = begin(&mut c, "/tx");
    c.rows(&tx, "CREATE (:T)", "{}");

    let path = tx.clone();
    let runner = std::thread::spawn(move || {
        let mut r = Client::connect(addr);
        r.request("POST", &path, Some(&slow_body(None)), &[])
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(!runner.is_finished());
    let started = Instant::now();
    let r = c.request("DELETE", &tx, None, &[]);
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"rolledBack":true}"#));
    let r = runner.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!((r.status, r.error_code().as_str()), (409, "Cancelled"), "{}", r.body);
    assert!(rolled_back(&r), "{}", r.body);

    assert_eq!(c.request("DELETE", &tx, None, &[]).status, 404);
    assert_eq!(count(&mut c, "/query", "T"), 0);
    c.rows("/query", "CREATE (:T)", "{}");
    assert_eq!(count(&mut c, "/query", "T"), 1);
}

#[test]
fn shutdown_cancels_running_transaction_statements() {
    let server = start(ServerConfig::default());
    let addr = server.local_addr();
    let mut c = Client::connect(addr);
    c.rows("/query", "UNWIND range(1, 1000) AS i CREATE (:N)", "{}");
    let tx = begin(&mut c, "/tx");
    let runner = std::thread::spawn(move || {
        let mut r = Client::connect(addr);
        r.request("POST", &tx, Some(&slow_body(None)), &[])
    });
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    drop(server);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let r = runner.join().unwrap();
    assert_eq!((r.status, r.error_code().as_str()), (409, "Cancelled"), "{}", r.body);
}

#[test]
fn shutdown_lets_running_queries_finish_within_the_grace_period() {
    // A query that ends on its own before the grace period passes is not cancelled: this
    // one ends with its own time limit.
    let server = start(ServerConfig {
        shutdown_grace: Duration::from_secs(10),
        ..ServerConfig::default()
    });
    let addr = server.local_addr();
    Client::connect(addr).rows("/query", "UNWIND range(1, 1000) AS i CREATE (:N)", "{}");
    let runner = std::thread::spawn(move || Client::connect(addr).request("POST", "/query", Some(&slow_body(Some(600))), &[]));
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    drop(server);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let r = runner.join().unwrap();
    assert_eq!((r.status, r.error_code().as_str()), (408, "Timeout"), "{}", r.body);

    // One that runs on is cancelled when the grace period ends.
    let server = start(ServerConfig {
        shutdown_grace: Duration::from_millis(300),
        ..ServerConfig::default()
    });
    let addr = server.local_addr();
    Client::connect(addr).rows("/query", "UNWIND range(1, 1000) AS i CREATE (:N)", "{}");
    let runner = std::thread::spawn(move || Client::connect(addr).request("POST", "/query", Some(&slow_body(None)), &[]));
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    drop(server);
    assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let r = runner.join().unwrap();
    assert_eq!((r.status, r.error_code().as_str()), (409, "Cancelled"), "{}", r.body);
}

// ---- backup, restore and check ----------------------------------------------------------

#[test]
fn backup_restore_and_check_endpoints() {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "graphbuff-server-backup-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mgr = Arc::new(DatabaseManager::open_with(dir.join("data"), StorageKind::persistent()).unwrap());
    let server = Server::start(
        mgr,
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            backup_dir: Some(dir.join("backups")),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mut c = Client::connect(server.local_addr());
    c.rows("/query", "UNWIND range(1, 50) AS i CREATE (:P {i: i})-[:R]->(:Q)", "{}");

    let r = c.request("POST", "/db/graphbuff/check", None, &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Object(check) = &m["check"] else { panic!() };
    assert_eq!(check["consistent"], Json::Bool(true));
    assert_eq!(check["nodes"], Json::Int(100));
    assert_eq!(check["errors"], Json::Array(vec![]));

    let r = c.request("POST", "/db/graphbuff/backup", Some(r#"{"backup": "first"}"#), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    let Json::Object(m) = r.json() else { panic!() };
    let Json::Object(b) = &m["backup"] else { panic!() };
    assert_eq!((&b["format"], &b["nodes"], &b["relationships"]), (&s("persistent"), &Json::Int(100), &Json::Int(50)));
    assert!(dir.join("backups").join("first").join("meta").is_file());

    // The name must stay inside the backup directory, and backups never overwrite.
    for bad in [r#"{"backup": "../x"}"#, r#"{"backup": ""}"#, r#"{"backup": 3}"#, r#"{}"#, r#"{"backup": "a", "x": 1}"#] {
        let r = c.request("POST", "/db/graphbuff/backup", Some(bad), &[]);
        assert_eq!((r.status, r.error_code().as_str()), (400, "BadRequest"), "{bad}: {}", r.body);
    }
    let r = c.request("POST", "/db/graphbuff/backup", Some(r#"{"backup": "first"}"#), &[]);
    assert_eq!(r.status, 400, "{}", r.body);
    let r = c.request("POST", "/db/nope/backup", Some(r#"{"backup": "x"}"#), &[]);
    assert_eq!((r.status, r.error_code().as_str()), (404, "DatabaseNotFound"));

    // Restore into a new database, then over a non-empty one.
    let r = c.request("POST", "/db/copy/restore", Some(r#"{"backup": "first"}"#), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        c.rows("/db/copy/query", "MATCH (p:P)-[:R]->(:Q) RETURN count(p)", "{}"),
        vec![Json::Array(vec![Json::Int(50)])]
    );
    let r = c.request("POST", "/db/copy/restore", Some(r#"{"backup": "first"}"#), &[]);
    assert_eq!((r.status, r.error_code().as_str()), (409, "ConstraintError"));
    let r = c.request("POST", "/db/copy/restore", Some(r#"{"backup": "first", "overwrite": true}"#), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    let r = c.request("POST", "/db/copy/restore", Some(r#"{"backup": "missing"}"#), &[]);
    assert_eq!(r.status, 404, "{}", r.body);
    let r = c.request("GET", "/db/copy/backup", None, &[]);
    assert_eq!(r.status, 405);
    drop(c);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backups_are_off_without_a_backup_dir() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    let r = c.request("POST", "/db/graphbuff/backup", Some(r#"{"backup": "x"}"#), &[]);
    assert_eq!((r.status, r.error_code().as_str()), (400, "Unsupported"));
    // In-memory databases cannot be checked.
    let r = c.request("POST", "/db/graphbuff/check", None, &[]);
    assert_eq!((r.status, r.error_code().as_str()), (400, "Unsupported"));
}

#[test]
fn test_procedures_only_with_the_flag() {
    let body = r#"{"name": "test.my.proc", "inputs": ["in"], "outputs": ["out"], "rows": [[1, "a"], [2, "b"], [1, "c"]]}"#;
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    assert_eq!(c.request("POST", "/db/graphbuff/test-procedure", Some(body), &[]).status, 404);

    let server = start(ServerConfig {
        test_procedures: true,
        ..ServerConfig::default()
    });
    let mut c = Client::connect(server.local_addr());
    let r = c.request("POST", "/db/graphbuff/test-procedure", Some(body), &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    let rows = c.rows("/db/graphbuff/query", "CALL test.my.proc(1) YIELD out RETURN out", "{}");
    assert_eq!(rows, vec![Json::Array(vec![s("a")]), Json::Array(vec![s("c")])]);
    // Another database does not have it.
    c.rows("/query", "CREATE DATABASE other", "{}");
    let r = c.query("/db/other/query", "CALL test.my.proc(1)", "{}");
    assert_ne!(r.status, 200);
}

#[test]
fn call_in_transactions_over_http() {
    let server = start(ServerConfig::default());
    let mut c = Client::connect(server.local_addr());
    let r = c.query(
        "/query",
        "UNWIND range(1, 25) AS i CALL { WITH i CREATE (:B {i: i}) } IN TRANSACTIONS OF 10 ROWS",
        "{}",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains(r#""nodesCreated":25"#), "{}", r.body);
    assert_eq!(count(&mut c, "/query", "B"), 25);

    // In an explicit transaction it is refused, and the transaction stays usable.
    let tx = begin(&mut c, "/tx");
    let r = c.query(&tx, "CALL { CREATE (:B) } IN TRANSACTIONS", "{}");
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("explicit transaction"), "{}", r.body);
    assert!(!rolled_back(&r), "{}", r.body);
    c.rows(&tx, "CREATE (:B)", "{}");
    let r = c.request("POST", &format!("{tx}/commit"), None, &[]);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(count(&mut c, "/query", "B"), 26);
}
