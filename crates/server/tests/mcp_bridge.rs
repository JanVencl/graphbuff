//! `graphbuff-server mcp`, the stdio bridge, against a running server. (A test binary of
//! its own: on Windows the child processes would inherit the sockets of tests running
//! beside them.)

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use graphbuff::DatabaseManager;
use graphbuff::cypher::ast::ExistingDatabase;
use graphbuff_server::json::{self, Json};
use graphbuff_server::{HashParams, McpConfig, Role, Server, ServerConfig, TokenAccess, Users};

const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"}}"#;
const SLOW: &str = "UNWIND range(1, 5000000) AS i UNWIND range(1, 1000) AS j WITH j WHERE j < 0 RETURN count(*) AS n";

fn users() -> Arc<Users> {
    let u = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
    u.create("rita", "ritapw", &[Role::Reader], ExistingDatabase::Fail).unwrap();
    Arc::new(u)
}

fn start(users: Arc<Users>, mcp: McpConfig) -> Server {
    Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            token: Some("tok".into()),
            users: Some(users),
            mcp: Some(McpConfig { bind: "127.0.0.1:0".into(), ..mcp }),
            ..ServerConfig::default()
        },
    )
    .unwrap()
}

/// The arguments `{"query": query}` with more fields (`rest`, an object's inside).
fn q(query: &str, rest: &str) -> String {
    let mut s = String::from(r#"{"query":"#);
    json::write_str(&mut s, query);
    if !rest.is_empty() {
        s.push(',');
        s.push_str(rest);
    }
    s.push('}');
    s
}

fn obj_of(text: &str) -> BTreeMap<String, Json> {
    match json::parse(text) {
        Ok(Json::Object(m)) => m,
        _ => panic!("not an object: {text}"),
    }
}

/// Runs `query` on the JSON API as the server token: the body of the answer.
fn http_query(server: &Server, query: &str) -> String {
    let addr = server.local_addr();
    let body = q(query, "");
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST /query HTTP/1.1
Host: {addr}
Connection: close
Authorization: Bearer tok
         Content-Type: application/json
Content-Length: {}

{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    answer
}

// ---- The stdio bridge ------------------------------------------------------------------------

/// `graphbuff-server mcp` for `url`, logged in with `token`.
struct Bridge {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Bridge {
    fn start(url: &str, token: &str) -> Bridge {
        use std::process::{Command, Stdio};
        let mut child = Command::new(env!("CARGO_BIN_EXE_graphbuff-server"))
            .args(["mcp", "--url", url])
            .env("GRAPHBUFF_MCP_TOKEN", token)
            .env_remove("GRAPHBUFF_MCP_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines() {
                let Ok(l) = l else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Bridge { stdin: child.stdin.take(), child, lines }
    }

    fn send(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{line}").unwrap();
        s.flush().unwrap();
    }

    /// The next message written, as an object.
    fn next(&self) -> BTreeMap<String, Json> {
        let l = self.lines.recv_timeout(Duration::from_secs(20)).expect("an answer from the bridge");
        obj_of(&l)
    }

    /// Closes its input; the bridge then ends.
    fn finish(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        for _ in 0..200 {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        panic!("the bridge did not end at the end of its input");
    }
}

fn result_text(m: &BTreeMap<String, Json>) -> String {
    let Some(Json::Object(r)) = m.get("result") else { panic!("no result: {m:?}") };
    let Some(Json::Array(c)) = r.get("content") else { panic!("no content: {r:?}") };
    let Some(Json::Object(c)) = c.first() else { panic!() };
    match c.get("text") {
        Some(Json::String(t)) => t.clone(),
        _ => panic!(),
    }
}

#[test]
fn stdio_bridge() {
    let users = users();
    let (rita, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig::default());
    let url = format!("http://{}/mcp", server.mcp_addr().unwrap());
    let mut b = Bridge::start(&url, &rita);

    // A legacy client: initialize, then requests that carry the agreed version.
    b.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#);
    let init = b.next();
    assert_eq!(init["id"], Json::Int(1));
    assert!(format!("{init:?}").contains("2025-06-18"), "{init:?}");
    b.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    b.send(r#"{"jsonrpc":"2.0","id":"two","method":"tools/call","params":{"name":"read_cypher","arguments":{"query":"RETURN 1 AS one"}}}"#);
    let r = b.next();
    assert_eq!(r["id"], Json::String("two".into()));
    assert!(result_text(&r).contains(r#""rows":[[1]]"#), "{r:?}");

    // A stateless client: the bridge adds the headers from the body.
    b.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"list_databases","arguments":{{}},{META}}}}}"#
    ));
    let r = b.next();
    assert!(result_text(&r).contains("graphbuff"), "{r:?}");

    // Not JSON: an error, and the bridge goes on.
    b.send("{nope");
    let r = b.next();
    assert_eq!(r["id"], Json::Null);
    assert!(format!("{:?}", r["error"]).contains("-32700"), "{r:?}");

    // A cancelled request gets no answer, and its query stops.
    b.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"read_cypher","arguments":{}}}}}"#,
        q(SLOW, "")
    ));
    let listed = || http_query(&server, "SHOW TRANSACTIONS YIELD protocol").contains(r#""mcp""#);
    let mut waited = 0;
    while !listed() {
        assert!(waited < 100, "the query never started");
        std::thread::sleep(Duration::from_millis(50));
        waited += 1;
    }
    b.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}}"#);
    let mut waited = 0;
    while listed() {
        assert!(waited < 200, "the cancelled query went on");
        std::thread::sleep(Duration::from_millis(50));
        waited += 1;
    }
    b.send(r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#);
    assert_eq!(b.next()["id"], Json::Int(5), "no answer for the cancelled request");
    assert!(b.finish().success());

    // A wrong token, and no server.
    let mut b = Bridge::start(&url, "gbt_wrong");
    b.send(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let r = b.next();
    assert_eq!(r["id"], Json::Int(1));
    assert!(format!("{r:?}").contains("GRAPHBUFF_MCP_TOKEN"), "{r:?}");
    b.finish();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut b = Bridge::start(&format!("http://{closed}/mcp"), &rita);
    b.send(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    let r = b.next();
    assert!(format!("{r:?}").contains("cannot reach"), "{r:?}");
    b.finish();

    // Without a token it says what to set.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_graphbuff-server"))
        .args(["mcp"])
        .env_remove("GRAPHBUFF_MCP_TOKEN")
        .output()
        .unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("GRAPHBUFF_MCP_TOKEN"));
}
