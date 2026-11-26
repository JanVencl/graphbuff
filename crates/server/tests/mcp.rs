//! The MCP endpoint: both protocol eras, headers, logins with bearer tokens, limits.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use graphbuff::DatabaseManager;
use graphbuff::cypher::ast::ExistingDatabase;
use graphbuff_server::json::{self, Json};
use graphbuff_server::{Cors, HashParams, McpConfig, Role, Server, ServerConfig, TokenAccess, Users};

fn users() -> Arc<Users> {
    let u = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
    u.create("root", "rootpw", &[Role::Admin], ExistingDatabase::Fail).unwrap();
    u.create("ed", "edpw", &[Role::Editor], ExistingDatabase::Fail).unwrap();
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

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> BTreeMap<String, Json> {
        match json::parse(&self.body) {
            Ok(Json::Object(m)) => m,
            _ => panic!("not a JSON object: {} {}", self.status, self.body),
        }
    }

    fn result(&self) -> BTreeMap<String, Json> {
        assert_eq!(self.status, 200, "{}", self.body);
        match self.json().remove("result") {
            Some(Json::Object(r)) => r,
            _ => panic!("no result: {}", self.body),
        }
    }

    fn error_code(&self) -> i64 {
        match &self.json()["error"] {
            Json::Object(e) => match e["code"] {
                Json::Int(c) => c,
                _ => panic!(),
            },
            _ => panic!("no error: {}", self.body),
        }
    }

    /// The text of a tools/call result, and whether it is an error.
    fn tool_text(&self) -> (String, bool) {
        let r = self.result();
        let Json::Array(content) = &r["content"] else { panic!("{}", self.body) };
        let Json::Object(first) = &content[0] else { panic!() };
        let Json::String(text) = &first["text"] else { panic!() };
        (text.clone(), r["isError"] == Json::Bool(true))
    }
}

/// POSTs (or sends `method`) `body` to `path` with `headers`.
fn send(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
        req.push_str(&format!("Host: {addr}\r\n"));
    }
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (n, v) = h.split_once(':').unwrap();
        headers.push((n.to_string(), v.trim().to_string()));
    }
    let mut body = String::new();
    r.read_to_string(&mut body).unwrap();
    Reply { status, headers, body }
}

const MODERN: &str = "2026-07-28";
const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"}}"#;

/// A legacy request (no `_meta` version) with `params` (an object's inside).
fn legacy(addr: SocketAddr, token: &str, method: &str, params: &str) -> Reply {
    let auth = format!("Bearer {token}");
    let body = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"{method}","params":{{{params}}}}}"#);
    send(addr, "POST", "/mcp", &[("Authorization", &auth), ("MCP-Protocol-Version", "2025-06-18")], &body)
}

/// A 2026-07-28 request with the headers it needs (`name` for `Mcp-Name`).
fn modern(addr: SocketAddr, token: &str, method: &str, name: Option<&str>, params: &str) -> Reply {
    let auth = format!("Bearer {token}");
    let mut headers = vec![("Authorization", auth.as_str()), ("MCP-Protocol-Version", MODERN), ("Mcp-Method", method)];
    if let Some(n) = name {
        headers.push(("Mcp-Name", n));
    }
    let sep = if params.is_empty() { "" } else { "," };
    let body = format!(r#"{{"jsonrpc":"2.0","id":"m1","method":"{method}","params":{{{params}{sep}{META}}}}}"#);
    send(addr, "POST", "/mcp", &headers, &body)
}

fn databases(text: &str) -> Vec<BTreeMap<String, Json>> {
    let Ok(Json::Object(mut m)) = json::parse(text) else { panic!("{text}") };
    let Some(Json::Array(dbs)) = m.remove("databases") else { panic!("{text}") };
    dbs.into_iter().map(|d| if let Json::Object(o) = d { o } else { panic!() }).collect()
}

#[test]
fn off_by_default() {
    let server = Server::start(Arc::new(DatabaseManager::in_memory()), ServerConfig { bind: "127.0.0.1:0".into(), ..ServerConfig::default() }).unwrap();
    assert_eq!(server.mcp_addr(), None);
}

#[test]
fn legacy_clients() {
    let users = users();
    let (token, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig::default());
    let addr = server.mcp_addr().unwrap();

    // initialize carries no version header yet.
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let r = send(addr, "POST", "/mcp", &[("Authorization", &format!("Bearer {token}"))], body);
    let init = r.result();
    assert_eq!(init["protocolVersion"], Json::String("2025-06-18".into()));
    assert!(r.header("mcp-session-id").is_none(), "stateless: no session");
    let Json::Object(info) = &init["serverInfo"] else { panic!() };
    assert_eq!(info["name"], Json::String("graphbuff".into()));
    assert!(matches!(&init["instructions"], Json::String(s) if s.contains("list_databases")));
    assert!(!init.contains_key("resultType"));
    // A version it does not know: the newest legacy one.
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#;
    let r = send(addr, "POST", "/mcp", &[("Authorization", &format!("Bearer {token}"))], body);
    assert_eq!(r.result()["protocolVersion"], Json::String("2025-11-25".into()));

    let auth = format!("Bearer {token}");
    let n = send(addr, "POST", "/mcp", &[("Authorization", &auth)], r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert_eq!((n.status, n.body.as_str()), (202, ""));
    assert!(legacy(addr, &token, "ping", "").result().is_empty());

    let tools = legacy(addr, &token, "tools/list", "").result();
    let Json::Array(list) = &tools["tools"] else { panic!() };
    let Json::Object(t) = &list[0] else { panic!() };
    assert_eq!(t["name"], Json::String("list_databases".into()));
    assert!(matches!(&t["annotations"], Json::Object(a) if a["readOnlyHint"] == Json::Bool(true)));
    assert!(!tools.contains_key("ttlMs"));

    let (text, err) = legacy(addr, &token, "tools/call", r#""name":"list_databases","arguments":{}"#).tool_text();
    assert!(!err, "{text}");
    let dbs = databases(&text);
    let default = dbs.iter().find(|d| d["default"] == Json::Bool(true)).unwrap();
    assert_eq!(default["access"], Json::String("read".into()));

    // Bad arguments are a result the model can correct; an unknown tool is a protocol error.
    let (text, err) = legacy(addr, &token, "tools/call", r#""name":"list_databases","arguments":{"x":1}"#).tool_text();
    assert!(err && text.contains("`x`"), "{text}");
    let r = legacy(addr, &token, "tools/call", r#""name":"drop_everything""#);
    assert_eq!((r.status, r.error_code()), (200, -32602));
    let r = legacy(addr, &token, "server/discover", "");
    assert_eq!((r.status, r.error_code()), (200, -32601));
    let r = legacy(addr, &token, "resources/read", r#""uri":"graphbuff://nothing""#);
    assert_eq!(r.error_code(), -32002);
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("MCP-Protocol-Version", "1999-01-01")], r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert_eq!((r.status, r.error_code()), (400, -32022));
}

#[test]
fn modern_clients() {
    let users = users();
    let (token, _) = users.create_token("ed", "t", TokenAccess::Write, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig::default());
    let addr = server.mcp_addr().unwrap();

    let d = modern(addr, &token, "server/discover", None, "").result();
    assert_eq!(d["resultType"], Json::String("complete".into()));
    assert_eq!(d["cacheScope"], Json::String("private".into()));
    assert!(matches!(d["ttlMs"], Json::Int(n) if n > 0));
    let Json::Array(versions) = &d["supportedVersions"] else { panic!() };
    assert_eq!(versions[0], Json::String(MODERN.into()));
    assert!(matches!(&d["_meta"], Json::Object(m) if m.contains_key("io.modelcontextprotocol/serverInfo")));

    let list = modern(addr, &token, "tools/list", None, "").result();
    assert!(list.contains_key("ttlMs"));
    let r = modern(addr, &token, "tools/call", Some("list_databases"), r#""name":"list_databases","arguments":{}"#);
    assert_eq!(r.result()["resultType"], Json::String("complete".into()));
    let (text, err) = r.tool_text();
    assert!(!err);
    assert_eq!(databases(&text)[0]["access"], Json::String("write".into()));
    // Mcp-Name in base64 ("list_databases").
    let r = modern(addr, &token, "tools/call", Some("=?base64?bGlzdF9kYXRhYmFzZXM=?="), r#""name":"list_databases""#);
    assert!(!r.tool_text().1);

    // The headers must repeat the body.
    let r = modern(addr, &token, "tools/call", None, r#""name":"list_databases""#);
    assert_eq!((r.status, r.error_code()), (400, -32020));
    let auth = format!("Bearer {token}");
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#);
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("MCP-Protocol-Version", MODERN)], &body);
    assert_eq!((r.status, r.error_code()), (400, -32020));
    let r = modern(addr, &token, "ping", None, "");
    assert_eq!((r.status, r.error_code()), (404, -32601));
    let r = modern(addr, &token, "resources/read", Some("graphbuff://x"), r#""uri":"graphbuff://x""#);
    assert_eq!(r.error_code(), -32602);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2030-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#;
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("MCP-Protocol-Version", "2030-01-01"), ("Mcp-Method", "tools/list")], body);
    assert_eq!((r.status, r.error_code()), (400, -32022));
    assert!(r.body.contains(r#""requested":"2030-01-01""#), "{}", r.body);
}

#[test]
fn logins_and_transport() {
    let users = users();
    let (rita, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let (root, _) = users.create_token("root", "t", TokenAccess::Write, &[], None).unwrap();
    let (dropped, _) = users.create_token("ed", "gone", TokenAccess::Read, &[], None).unwrap();
    let mcp = McpConfig { allowed_origins: Cors::parse("https://app.example.com").unwrap(), ..McpConfig::default() };
    let server = start(Arc::clone(&users), mcp);
    let addr = server.mcp_addr().unwrap();
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    let r = send(addr, "POST", "/mcp", &[], list);
    assert_eq!((r.status, r.header("www-authenticate")), (401, Some("Bearer")));
    // Passwords are not accepted, not even as a bearer token.
    let r = send(addr, "POST", "/mcp", &[("Authorization", "Basic cm9vdDpyb290cHc=")], list);
    assert_eq!(r.status, 401);
    assert_eq!(send(addr, "POST", "/mcp", &[("Authorization", "Bearer rootpw")], list).status, 401);
    // The server token works, but is capped at editor like everyone.
    let (text, _) = legacy(addr, "tok", "tools/call", r#""name":"list_databases""#).tool_text();
    assert_eq!(databases(&text)[0]["access"], Json::String("write".into()));
    let (text, _) = legacy(addr, &root, "tools/call", r#""name":"list_databases""#).tool_text();
    assert_eq!(databases(&text)[0]["access"], Json::String("write".into()));
    assert_eq!(legacy(addr, &rita, "tools/list", "").status, 200);
    users.drop_token("ed", "gone").unwrap();
    assert_eq!(legacy(addr, &dropped, "tools/list", "").status, 401);

    let auth = format!("Bearer {rita}");
    let r = send(addr, "GET", "/mcp", &[("Authorization", &auth)], "");
    assert_eq!((r.status, r.header("allow")), (405, Some("POST")));
    assert_eq!(send(addr, "DELETE", "/mcp", &[("Authorization", &auth)], "").status, 405);
    assert_eq!(send(addr, "POST", "/other", &[("Authorization", &auth)], list).status, 404);
    // Browsers: only allowed origins; DNS rebinding: only this machine's names.
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("Origin", "https://evil.example")], list);
    assert_eq!(r.status, 403);
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("Origin", "https://app.example.com")], list);
    assert_eq!((r.status, r.header("access-control-allow-origin")), (200, Some("https://app.example.com")));
    let pre = [("Origin", "https://app.example.com"), ("Access-Control-Request-Method", "POST")];
    let r = send(addr, "OPTIONS", "/mcp", &pre, "");
    assert_eq!(r.status, 204);
    assert!(r.header("access-control-allow-headers").unwrap().contains("Mcp-Method"));
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("Host", "evil.example:7476")], list);
    assert_eq!(r.status, 403);
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth), ("Host", &format!("localhost:{}", addr.port()))], list);
    assert_eq!(r.status, 200);

    // Malformed messages.
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth)], "{not json");
    assert_eq!((r.status, r.error_code()), (400, -32700));
    let r = send(addr, "POST", "/mcp", &[("Authorization", &auth)], &format!("[{list}]"));
    assert_eq!((r.status, r.error_code()), (400, -32600));

    // The JSON API's port has no /mcp.
    assert_eq!(send(server.local_addr(), "POST", "/mcp", &[("Authorization", "Bearer tok")], list).status, 404);
}

#[test]
fn limits_and_metrics() {
    let users = users();
    let (token, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig { calls_per_minute: 2, ..McpConfig::default() });
    let addr = server.mcp_addr().unwrap();
    let call = r#""name":"list_databases""#;
    assert!(!legacy(addr, &token, "tools/call", call).tool_text().1);
    assert!(!legacy(addr, &token, "tools/call", call).tool_text().1);
    let (text, err) = legacy(addr, &token, "tools/call", call).tool_text();
    assert!(err && text.contains("rate limit"), "{text}");
    // Other calls are not limited, nor other tokens.
    assert_eq!(legacy(addr, &token, "tools/list", "").status, 200);
    assert!(!legacy(addr, "tok", "tools/call", call).tool_text().1);
    assert_eq!(legacy(addr, "wrong", "tools/list", "").status, 401);

    let m = send(server.local_addr(), "GET", "/metrics", &[("Authorization", "Bearer tok")], "");
    assert_eq!(m.status, 200);
    for line in [
        r#"graphbuff_mcp_tool_calls_total{tool="list_databases",outcome="ok"} 3"#,
        r#"graphbuff_mcp_tool_calls_total{tool="list_databases",outcome="limited"} 1"#,
        r#"graphbuff_mcp_tool_call_duration_seconds_count{tool="list_databases"} 4"#,
        r#"graphbuff_failed_logins_total{protocol="mcp"} 1"#,
    ] {
        assert!(m.body.lines().any(|l| l == line), "no `{line}` in\n{}", m.body);
    }
}

// ---- Cypher tools ---------------------------------------------------------------------------

/// Calls `tool` with `args` (a JSON object) as a legacy client: its text, and whether it is
/// an error.
fn call(addr: SocketAddr, token: &str, tool: &str, args: &str) -> (String, bool) {
    legacy(addr, token, "tools/call", &format!(r#""name":"{tool}","arguments":{args}"#)).tool_text()
}

/// The arguments `{"query": query}` with more fields (`rest`, an object's inside, may be
/// empty).
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

/// Runs `query` on the JSON API as the server token.
fn http_query(server: &Server, query: &str) -> String {
    let r = send(server.local_addr(), "POST", "/query", &[("Authorization", "Bearer tok")], &q(query, ""));
    assert_eq!(r.status, 200, "{}", r.body);
    r.body
}

fn tool_names(addr: SocketAddr, token: &str) -> Vec<String> {
    let Some(Json::Array(list)) = legacy(addr, token, "tools/list", "").result().remove("tools") else { panic!() };
    list.into_iter()
        .map(|t| match t {
            Json::Object(mut t) => match t.remove("name") {
                Some(Json::String(n)) => n,
                _ => panic!(),
            },
            _ => panic!(),
        })
        .collect()
}

#[test]
fn reading() {
    let users = users();
    let (rita, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig::default());
    let addr = server.mcp_addr().unwrap();
    http_query(&server, "UNWIND range(1, 300) AS i CREATE (:P {i: i, v: [x IN range(1, 40) | toFloat(x)], name: 'p' + i})");

    assert_eq!(tool_names(addr, &rita), ["list_databases", "get_schema", "read_cypher"]);
    let (text, err) = call(addr, &rita, "read_cypher", &q("MATCH (p:P) WHERE p.i = $i RETURN p.name AS name, p.v AS v", r#""params":{"i":7}"#));
    assert!(!err, "{text}");
    assert_eq!(text, r#"{"database":"graphbuff","columns":["name","v"],"rows":[["p7","<vector 40>"]]}"#);

    // Capped: 100 rows by default, or what the call asks, up to the limit.
    let r = obj_of(&call(addr, &rita, "read_cypher", &q("MATCH (p:P) RETURN p.i AS i", "")).0);
    assert_eq!((&r["truncated"], &r["shown"]), (&Json::Bool(true), &Json::Int(100)));
    assert!(matches!(&r["hint"], Json::String(h) if h.contains("LIMIT")));
    let r = obj_of(&call(addr, &rita, "read_cypher", &q("MATCH (p:P) RETURN p.i AS i", r#""max_rows":5"#)).0);
    assert_eq!(r["shown"], Json::Int(5));
    let (text, err) = call(addr, &rita, "read_cypher", &q("RETURN 1", r#""max_rows":5000"#));
    assert!(err && text.contains("1 to 1000"), "{text}");
    let r = obj_of(&call(addr, &rita, "read_cypher", &q("MATCH (p:P) RETURN count(*) AS n", "")).0);
    assert_eq!(r["rows"], json::parse("[[300]]").unwrap());
    assert!(!r.contains_key("truncated"));
    // EXPLAIN of a write is a read; the plan comes along.
    let r = obj_of(&call(addr, &rita, "read_cypher", &q("EXPLAIN CREATE (:X)", "")).0);
    assert!(r.contains_key("plan"), "{r:?}");

    // Errors are results the model can correct.
    let (text, err) = call(addr, &rita, "read_cypher", &q("CREATE (:X)", ""));
    assert!(err && text.contains("write_cypher"), "{text}");
    let (text, err) = call(addr, &rita, "read_cypher", &q("MATCH (a)-[:R]->{1,3}(b) RETURN b", ""));
    assert!(err && text.starts_with("SyntaxError") && text.contains("[:R*1..3]"), "{text}");
    let (text, err) = call(addr, &rita, "read_cypher", &q("RETURN 1", r#""database":"nope""#));
    assert!(err && text.contains("`nope` does not exist"), "{text}");
    let (text, err) = call(addr, &rita, "read_cypher", &q("RETURN 1", r#""dry_run":true"#));
    assert!(err && text.contains("unknown argument `dry_run`"), "{text}");
    let (text, err) = call(addr, &rita, "read_cypher", "{}");
    assert!(err && text.contains("`query`"), "{text}");
    // A reader's token does not see write_cypher, and cannot call it.
    let r = legacy(addr, &rita, "tools/call", r#""name":"write_cypher","arguments":{"query":"CREATE ()"}"#);
    assert_eq!(r.error_code(), -32602);
}

#[test]
fn writing_and_what_mcp_refuses() {
    let users = users();
    let (ed, _) = users.create_token("ed", "t", TokenAccess::Write, &[], None).unwrap();
    let (ed_read, _) = users.create_token("ed", "r", TokenAccess::Read, &[], None).unwrap();
    let (root, _) = users.create_token("root", "t", TokenAccess::Write, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig::default());
    let addr = server.mcp_addr().unwrap();
    http_query(&server, "CREATE DATABASE other");
    let (scoped, _) = users.create_token("ed", "s", TokenAccess::Write, &["graphbuff".into()], None).unwrap();

    assert_eq!(tool_names(addr, &ed), ["list_databases", "get_schema", "read_cypher", "write_cypher"]);
    // An editor's read-only token sees no write tool.
    assert_eq!(tool_names(addr, &ed_read), ["list_databases", "get_schema", "read_cypher"]);

    let r = obj_of(&call(addr, &ed, "write_cypher", &q("UNWIND range(1, 3) AS i CREATE (n:W {i: i}) RETURN n.i AS i", "")).0);
    assert_eq!(r["stats"], json::parse(r#"{"labelsAdded":3,"nodesCreated":3,"propertiesSet":3}"#).unwrap());
    // A dry run reports and rolls back.
    let (text, err) = call(addr, &ed, "write_cypher", &q("MATCH (n:W) DETACH DELETE n", r#""dry_run":true"#));
    assert!(!err, "{text}");
    let r = obj_of(&text);
    assert_eq!((&r["dryRun"], &r["rolledBack"]), (&Json::Bool(true), &Json::Bool(true)));
    assert_eq!(r["stats"], json::parse(r#"{"nodesDeleted":3}"#).unwrap());
    let count = |db: &str| {
        let args = q("MATCH (n:W) RETURN count(n) AS n", &format!(r#""database":"{db}""#));
        obj_of(&call(addr, &ed, "read_cypher", &args).0)["rows"].clone()
    };
    assert_eq!(count("graphbuff"), json::parse("[[3]]").unwrap());
    let (text, err) = call(addr, &ed, "write_cypher", &q("CREATE INDEX w_i FOR (n:W) ON (n.i)", r#""dry_run":true"#));
    assert!(!err, "{text}");
    assert!(!http_query(&server, "SHOW INDEXES").contains("w_i"), "a dry run leaves no index");
    let (text, err) = call(addr, &ed, "write_cypher", &q("CREATE INDEX w_i FOR (n:W) ON (n.i)", ""));
    assert!(!err && text.contains(r#""indexesAdded":1"#), "{text}");
    let in_tx = "MATCH (n:W) CALL { WITH n SET n.x = 1 } IN TRANSACTIONS RETURN 1";
    let (text, err) = call(addr, &ed, "write_cypher", &q(in_tx, r#""dry_run":true"#));
    assert!(err && text.contains("dry run"), "{text}");
    // Writes on another database, by name or USE; not for a token limited to one.
    call(addr, &ed, "write_cypher", &q("CREATE (:W)", r#""database":"other""#));
    assert_eq!(count("other"), json::parse("[[1]]").unwrap());
    let (text, err) = call(addr, &scoped, "read_cypher", &q("USE other MATCH (n) RETURN n", ""));
    assert!(err && text.starts_with("Forbidden"), "{text}");

    // Not over MCP, whatever the role.
    for (query, what) in [
        ("CREATE DATABASE x", "administration"),
        ("DROP DATABASE other", "administration"),
        ("CREATE USER bob SET PASSWORD 'secret12'", "administration"),
        ("GRANT ROLE admin TO ed", "administration"),
        ("CREATE TOKEN more", "administration"),
        ("SHOW USERS", "administration"),
        ("SHOW DATABASES", "list_databases"),
        ("SHOW SETTINGS", "not available"),
        ("TERMINATE TRANSACTIONS 'graphbuff-transaction-1'", "not available"),
        ("SHOW TRANSACTIONS", "not available"),
        ("USE other", "USE on its own"),
        ("LOAD CSV FROM 'https://example.com/a.csv' AS r CREATE (:R {v: r[0]})", "LOAD CSV"),
        ("MATCH (n) CALL { LOAD CSV FROM 'file:///etc/passwd' AS r RETURN r } RETURN r", "LOAD CSV"),
    ] {
        let (text, err) = call(addr, &root, "write_cypher", &q(query, ""));
        assert!(err && text.contains(what), "{query}: {text}");
    }
    assert!(!http_query(&server, "SHOW DATABASES").contains("\"x\""));
}

const SLOW: &str = "UNWIND range(1, 5000000) AS i UNWIND range(1, 1000) AS j WITH j WHERE j < 0 RETURN count(*) AS n";

#[test]
fn time_limit() {
    let users = users();
    let (rita, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig { query_timeout: Duration::from_millis(300), ..McpConfig::default() });
    let (text, err) = call(server.mcp_addr().unwrap(), &rita, "read_cypher", &q(SLOW, ""));
    assert!(err && text.starts_with("Timeout"), "{text}");
}

#[test]
fn a_client_that_leaves_cancels_its_query() {
    let users = users();
    let (rita, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig { query_timeout: Duration::from_secs(60), ..McpConfig::default() });
    let addr = server.mcp_addr().unwrap();
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"read_cypher","arguments":{}}}}}"#, q(SLOW, ""));
    let mut s = TcpStream::connect(addr).unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {rita}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let listed = || http_query(&server, "SHOW TRANSACTIONS YIELD protocol, currentQuery").contains(r#""mcp""#);
    let mut waited = 0;
    while !listed() {
        assert!(waited < 100, "the query never showed in SHOW TRANSACTIONS");
        std::thread::sleep(Duration::from_millis(50));
        waited += 1;
    }
    drop(s);
    let mut waited = 0;
    while listed() {
        assert!(waited < 200, "the query was not cancelled when its client left");
        std::thread::sleep(Duration::from_millis(50));
        waited += 1;
    }
    let m = send(server.local_addr(), "GET", "/metrics", &[("Authorization", "Bearer tok")], "");
    assert!(m.body.contains(r#"graphbuff_queries_total{protocol="mcp",outcome="cancelled"} 1"#), "{}", m.body);
}

#[test]
fn schema() {
    let users = users();
    let (rita, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let (scoped, _) = users.create_token("rita", "s", TokenAccess::Read, &["graphbuff".into()], None).unwrap();
    let server = start(Arc::clone(&users), McpConfig::default());
    let addr = server.mcp_addr().unwrap();
    http_query(&server, "CREATE DATABASE other");
    http_query(
        &server,
        "CREATE (a:Person {name: 'Keanu', born: 1964})-[:ACTED_IN {roles: ['Neo']}]->(m:Movie {title: 'The Matrix'}), \
         (:Person {name: 'Lana'})-[:DIRECTED]->(m), (:`Odd label` {x: 1.5})",
    );
    http_query(&server, "CREATE INDEX person_name FOR (p:Person) ON (p.name)");

    let (text, err) = call(addr, &rita, "get_schema", "{}");
    assert!(!err, "{text}");
    let r = obj_of(&text);
    assert_eq!(r["database"], Json::String("graphbuff".into()));
    assert_eq!(
        r["labels"],
        json::parse(
            r#"[{"count":1,"label":"Movie","properties":{"title":"String"}},
                {"count":1,"label":"Odd label","properties":{"x":"Double"}},
                {"count":2,"label":"Person","properties":{"born":"Long?","name":"String"}}]"#
        )
        .unwrap()
    );
    assert_eq!(
        r["patterns"],
        json::parse(r#"["(:Person)-[:ACTED_IN]->(:Movie)","(:Person)-[:DIRECTED]->(:Movie)"]"#).unwrap()
    );
    assert_eq!(
        r["relationshipTypes"],
        json::parse(r#"[{"count":1,"properties":{"roles":"StringArray"},"type":"ACTED_IN"},{"count":1,"properties":{},"type":"DIRECTED"}]"#)
            .unwrap()
    );
    assert!(text.contains("person_name"), "{text}");

    // Some labels only; an unknown one is an error that lists them.
    let r = obj_of(&call(addr, &rita, "get_schema", r#"{"labels":["Odd label"]}"#).0);
    assert_eq!(r["patterns"], Json::Array(vec![]));
    assert_eq!(r["relationshipTypes"], Json::Array(vec![]));
    assert!(!format!("{:?}", r["indexes"]).contains("person_name"));
    let (text, err) = call(addr, &rita, "get_schema", r#"{"labels":["Nope"]}"#);
    assert!(err && text.contains("Movie, Odd label, Person"), "{text}");

    // Cached, but not past a change in the counts.
    http_query(&server, "CREATE (:Studio {name: 'WB'})");
    assert!(call(addr, &rita, "get_schema", "{}").0.contains("Studio"));

    // Only databases the token may read.
    let (text, err) = call(addr, &scoped, "get_schema", r#"{"database":"other"}"#);
    assert!(err && text.contains("may not read"), "{text}");
    let (text, err) = call(addr, &rita, "get_schema", r#"{"database":"other"}"#);
    assert!(!err && text.contains(r#""labels":[]"#), "{text}");
    let (text, err) = call(addr, &rita, "get_schema", r#"{"x":1}"#);
    assert!(err && text.contains("unknown argument"), "{text}");
}
