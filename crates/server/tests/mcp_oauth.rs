//! OAuth on the MCP port: discovery, the login and consent page, codes with PKCE,
//! rotating refresh tokens, revocation, and where the tokens work.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use graphbuff::DatabaseManager;
use graphbuff::cypher::ast::ExistingDatabase;
use graphbuff_server::json::{self, Json};
use graphbuff_server::{HashParams, McpConfig, OAuthClient, Role, Server, ServerConfig, Users};

const URL: &str = "https://mcp.test/mcp";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT: &str = "http://localhost:5555/cb";

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

    /// A field of the JSON body as text.
    fn field(&self, k: &str) -> String {
        match self.json().remove(k) {
            Some(Json::String(s)) => s,
            other => panic!("no `{k}` in {} ({other:?})", self.body),
        }
    }
}

fn send(addr: SocketAddr, method: &str, target: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).unwrap();
    let mut all = String::new();
    s.read_to_string(&mut all).unwrap();
    let (head, body) = all.split_once("\r\n\r\n").unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers =
        lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_string(), v.trim().to_string())).collect();
    Reply { status, headers, body: body.to_string() }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn form(fields: &[(&str, &str)]) -> String {
    fields.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&")
}

/// The value of query parameter `k` in a URL.
fn param(url: &str, k: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|p| {
        let (n, v) = p.split_once('=')?;
        (n == k).then(|| {
            let mut out = Vec::new();
            let b = v.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' {
                    out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).unwrap()
        })
    })
}

struct Test {
    server: Server,
    mcp: SocketAddr,
    users: Arc<Users>,
}

fn start() -> Test {
    let u = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
    u.create("root", "rootpw", &[Role::Admin], ExistingDatabase::Fail).unwrap();
    u.create("ann", "annpw", &[Role::Editor], ExistingDatabase::Fail).unwrap();
    u.create("rita", "ritapw", &[Role::Reader], ExistingDatabase::Fail).unwrap();
    let users = Arc::new(u);
    let server = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            token: Some("tok".into()),
            users: Some(Arc::clone(&users)),
            mcp: Some(McpConfig {
                bind: "127.0.0.1:0".into(),
                public_url: Some(URL.into()),
                oauth_clients: vec![OAuthClient { id: "app".into(), redirect_uris: vec!["http://localhost/cb".into()] }],
                ..McpConfig::default()
            }),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let mcp = server.mcp_addr().unwrap();
    Test { server, mcp, users }
}

impl Test {
    fn authorize_url(&self, extra: &str) -> String {
        format!(
            "/oauth/authorize?response_type=code&client_id=app&redirect_uri={}&code_challenge={CHALLENGE}\
             &code_challenge_method=S256&state=xyz&resource={}{extra}",
            enc(REDIRECT),
            enc(URL)
        )
    }

    /// Opens the consent page: the form's signed request and the cookie.
    fn page(&self, scope: &str) -> (String, String) {
        let r = send(self.mcp, "GET", &self.authorize_url(&format!("&scope={}", enc(scope))), &[], "");
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(r.body.contains("Connect an app") && r.body.contains("localhost"), "{}", r.body);
        assert_eq!(r.header("X-Frame-Options"), Some("DENY"));
        let cookie = r.header("Set-Cookie").unwrap().split(';').next().unwrap().to_string();
        assert!(cookie.starts_with("gbo_"), "{cookie}");
        let request = r.body.split("name=request value=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        (request, cookie)
    }

    fn answer(&self, id: &str, cookie: &str, fields: &[(&str, &str)]) -> Reply {
        let mut all = vec![("request", id)];
        all.extend_from_slice(fields);
        send(
            self.mcp,
            "POST",
            "/oauth/authorize",
            &[("Content-Type", "application/x-www-form-urlencoded"), ("Cookie", cookie), ("Origin", "https://mcp.test")],
            &form(&all),
        )
    }

    /// A whole login as `user`: the code.
    fn code(&self, user: &str, password: &str, access: &str) -> String {
        let (id, cookie) = self.page("read write");
        let r = self.answer(&id, &cookie, &[("user", user), ("password", password), ("access", access), ("action", "allow")]);
        assert_eq!(r.status, 302, "{}", r.body);
        let to = r.header("Location").unwrap();
        assert!(to.starts_with("http://localhost:5555/cb?"), "{to}");
        assert_eq!(param(to, "state").as_deref(), Some("xyz"));
        assert_eq!(param(to, "iss").as_deref(), Some("https://mcp.test"));
        param(to, "code").unwrap()
    }

    fn token(&self, fields: &[(&str, &str)]) -> Reply {
        send(self.mcp, "POST", "/oauth/token", &[("Content-Type", "application/x-www-form-urlencoded")], &form(fields))
    }

    fn exchange(&self, code: &str) -> Reply {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("client_id", "app"),
            ("code_verifier", VERIFIER),
            ("resource", URL),
        ])
    }

    fn refresh(&self, refresh: &str) -> Reply {
        self.token(&[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", "app")])
    }

    /// `tools/list` with `token`: the status, and the tool names.
    fn tools(&self, token: &str) -> (u16, String) {
        let auth = format!("Bearer {token}");
        let r = send(
            self.mcp,
            "POST",
            "/mcp",
            &[("Authorization", &auth), ("Content-Type", "application/json")],
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        (r.status, r.body)
    }
}

#[test]
fn discovery() {
    let t = start();
    let r = send(t.mcp, "GET", "/.well-known/oauth-protected-resource/mcp", &[], "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.field("resource"), URL);
    assert!(r.body.contains(r#""authorization_servers":["https://mcp.test"]"#), "{}", r.body);
    assert_eq!(send(t.mcp, "GET", "/.well-known/oauth-protected-resource", &[], "").status, 200);
    for path in ["/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"] {
        let r = send(t.mcp, "GET", path, &[], "");
        assert_eq!(r.field("issuer"), "https://mcp.test");
        assert_eq!(r.field("token_endpoint"), "https://mcp.test/oauth/token");
        assert!(r.body.contains(r#""code_challenge_methods_supported":["S256"]"#), "{}", r.body);
        assert!(r.body.contains(r#""client_id_metadata_document_supported":true"#), "{}", r.body);
        assert!(r.body.contains(r#""token_endpoint_auth_methods_supported":["none"]"#), "{}", r.body);
    }
    // Without a token: where to log in.
    let r = send(t.mcp, "POST", "/mcp", &[("Content-Type", "application/json")], r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    assert_eq!(r.status, 401);
    assert_eq!(
        r.header("WWW-Authenticate"),
        Some(r#"Bearer resource_metadata="https://mcp.test/.well-known/oauth-protected-resource/mcp", scope="read""#)
    );
    // The public host is accepted by the loopback Host check.
    let mut s = TcpStream::connect(t.mcp).unwrap();
    s.write_all(b"GET /.well-known/oauth-authorization-server HTTP/1.1\r\nHost: mcp.test\r\nConnection: close\r\n\r\n").unwrap();
    let mut all = String::new();
    s.read_to_string(&mut all).unwrap();
    assert!(all.starts_with("HTTP/1.1 200"), "{all}");
}

#[test]
fn login_consent_and_tokens() {
    let t = start();
    // Bad requests are shown on the page, not sent to an unchecked redirect URI.
    let r = send(t.mcp, "GET", &t.authorize_url("").replace("client_id=app", "client_id=nobody"), &[], "");
    assert!(r.status == 400 && r.body.contains("unknown client"), "{}", r.body);
    let r = send(t.mcp, "GET", &t.authorize_url("").replace(&enc(REDIRECT), &enc("https://evil.example/cb")), &[], "");
    assert!(r.status == 400 && r.header("Location").is_none(), "{}", r.body);
    // Once the redirect URI is good, errors go back to the client.
    let r = send(t.mcp, "GET", &t.authorize_url("").replace("&code_challenge_method=S256", ""), &[], "");
    assert_eq!(r.status, 302);
    assert_eq!(param(r.header("Location").unwrap(), "error").as_deref(), Some("invalid_request"));
    let bad = format!("{}~", &CHALLENGE[..42]);
    let r = send(t.mcp, "GET", &t.authorize_url("").replace(CHALLENGE, &bad), &[], "");
    assert_eq!(param(r.header("Location").unwrap(), "error").as_deref(), Some("invalid_request"), "not base64url");
    let r = send(t.mcp, "GET", &t.authorize_url("").replace(&enc(URL), &enc("https://other.test/mcp")), &[], "");
    assert_eq!(param(r.header("Location").unwrap(), "error").as_deref(), Some("invalid_target"));

    // The form needs its cookie, and the right password.
    let (id, cookie) = t.page("read");
    let r = send(
        t.mcp,
        "POST",
        "/oauth/authorize",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        &form(&[("request", &id), ("user", "ann"), ("password", "annpw"), ("action", "allow")]),
    );
    assert_eq!(r.status, 403, "{}", r.body);
    // Another page's cookie does not fit.
    let (_, other) = t.page("read");
    let r = t.answer(&id, &other, &[("user", "ann"), ("password", "annpw"), ("action", "allow")]);
    assert_eq!(r.status, 403, "{}", r.body);
    // A changed request is refused.
    let r = t.answer(&id.replacen('e', "f", 1).replacen('A', "B", 1), &cookie, &[("action", "deny")]);
    assert!(r.status == 400 && r.body.contains("expired"), "{}", r.body);
    let r = t.answer(&id, &cookie, &[("user", "ann"), ("password", "wrong"), ("action", "allow")]);
    assert!(r.status == 401 && r.body.contains("wrong user name or password"), "{}", r.body);
    // Deny.
    let r = t.answer(&id, &cookie, &[("action", "deny")]);
    assert_eq!(r.status, 302);
    assert_eq!(param(r.header("Location").unwrap(), "error").as_deref(), Some("access_denied"));

    // Allow, and exchange the code.
    let code = t.code("ann", "annpw", "write");
    let r = t.token(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", REDIRECT),
        ("client_id", "app"),
        ("code_verifier", "x".repeat(43).as_str()),
    ]);
    assert!(r.status == 400 && r.field("error") == "invalid_grant", "{}", r.body);
    // A code that failed once is spent.
    assert_eq!(t.exchange(&code).field("error"), "invalid_grant");

    // A page gives one code.
    let (id, cookie) = t.page("read");
    let allow = [("user", "ann"), ("password", "annpw"), ("action", "allow")];
    assert_eq!(t.answer(&id, &cookie, &allow).status, 302);
    let r = t.answer(&id, &cookie, &allow);
    assert!(r.status == 400 && r.body.contains("answered already"), "{}", r.body);

    let code = t.code("ann", "annpw", "write");
    let r = t.exchange(&code);
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("Cache-Control"), Some("no-store"));
    assert_eq!(r.field("token_type"), "Bearer");
    assert_eq!(r.field("scope"), "write");
    assert_eq!(r.json()["expires_in"], Json::Int(3600));
    let (a1, r1) = (r.field("access_token"), r.field("refresh_token"));
    let (status, tools) = t.tools(&a1);
    assert!(status == 200 && tools.contains(r#""name":"write_cypher""#), "{tools}");
    // Only at the MCP endpoint.
    let auth = format!("Bearer {a1}");
    let r = send(t.server.local_addr(), "POST", "/query", &[("Authorization", &auth), ("Content-Type", "application/json")], r#"{"query":"RETURN 1"}"#);
    assert_eq!(r.status, 401, "{}", r.body);
    // Listed as a token of ann, from client app.
    let listed = t.users.tokens(Some("ann"));
    assert!(listed.iter().any(|i| i.client.as_deref() == Some("app") && i.audience.as_deref() == Some(URL)), "{listed:?}");

    // A refresh replaces both tokens.
    let r = t.refresh(&r1);
    assert_eq!(r.status, 200, "{}", r.body);
    let (a2, r2) = (r.field("access_token"), r.field("refresh_token"));
    assert_eq!(t.tools(&a1).0, 401);
    assert_eq!(t.tools(&a2).0, 200);
    // Another client cannot use it (and that does not end the grant).
    let r = t.token(&[("grant_type", "refresh_token"), ("refresh_token", &r2), ("client_id", "other")]);
    assert_eq!(r.field("error"), "invalid_grant");
    assert_eq!(t.tools(&a2).0, 200);
    assert_eq!(t.refresh(&r2).status, 200);
}

#[test]
fn stolen_tokens() {
    let t = start();
    // An old refresh token used again ends the grant.
    let r = t.exchange(&t.code("ann", "annpw", "read"));
    let r1 = r.field("refresh_token");
    let r = t.refresh(&r1);
    let (a2, r2) = (r.field("access_token"), r.field("refresh_token"));
    assert_eq!(t.tools(&a2).0, 200);
    assert_eq!(t.refresh(&r1).field("error"), "invalid_grant");
    assert_eq!(t.tools(&a2).0, 401);
    assert_eq!(t.refresh(&r2).field("error"), "invalid_grant");

    // A code used twice ends what it gave.
    let code = t.code("ann", "annpw", "read");
    let a = t.exchange(&code).field("access_token");
    assert_eq!(t.tools(&a).0, 200);
    assert_eq!(t.exchange(&code).field("error"), "invalid_grant");
    assert_eq!(t.tools(&a).0, 401);

    // Revoking the refresh token ends the grant; so does DROP TOKEN.
    let r = t.exchange(&t.code("ann", "annpw", "read"));
    let (a, refresh) = (r.field("access_token"), r.field("refresh_token"));
    let r = send(t.mcp, "POST", "/oauth/revoke", &[("Content-Type", "application/x-www-form-urlencoded")], &form(&[("token", &refresh)]));
    assert_eq!(r.status, 200);
    assert_eq!(t.tools(&a).0, 401);
    let r = t.exchange(&t.code("ann", "annpw", "read"));
    let (a, refresh) = (r.field("access_token"), r.field("refresh_token"));
    let name = t.users.tokens(Some("ann")).into_iter().find(|i| i.client.is_some()).unwrap().name;
    assert!(t.users.drop_token("ann", &name).unwrap());
    assert_eq!(t.tools(&a).0, 401);
    assert_eq!(t.refresh(&refresh).field("error"), "invalid_grant");

    // A reader asking for write gets read.
    let r = t.exchange(&t.code("rita", "ritapw", "write"));
    assert_eq!(r.field("scope"), "read");
    let (status, tools) = t.tools(&r.field("access_token"));
    assert!(status == 200 && !tools.contains(r#""name":"write_cypher""#), "{tools}");
}
