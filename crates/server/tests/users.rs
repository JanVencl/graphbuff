//! User accounts and roles, over HTTP and Bolt.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use graphbuff::DatabaseManager;
use graphbuff::cypher::ast::ExistingDatabase;
use graphbuff_server::json::{self, Json};
use graphbuff_server::packstream::{self, PackValue};
use graphbuff_server::{HashParams, LoginLimits, Role, Server, ServerConfig, TokenAccess, Users};

fn users() -> Arc<Users> {
    let u = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
    u.create("root", "rootpw", &[Role::Admin], ExistingDatabase::Fail).unwrap();
    u.create("ed", "edpw", &[Role::Editor], ExistingDatabase::Fail).unwrap();
    u.create("rita", "ritapw", &[Role::Reader], ExistingDatabase::Fail).unwrap();
    u.create("nobody", "nopw", &[], ExistingDatabase::Fail).unwrap();
    Arc::new(u)
}

fn start(users: Arc<Users>, token: Option<&str>) -> Server {
    Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            bolt: Some("127.0.0.1:0".into()),
            token: token.map(String::from),
            users: Some(users),
            ..ServerConfig::default()
        },
    )
    .unwrap()
}

// ---- HTTP ---------------------------------------------------------------------------------

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn code(&self) -> String {
        let Ok(Json::Object(m)) = json::parse(&self.body) else { panic!("{}", self.body) };
        match &m["error"] {
            Json::Object(e) => match &e["code"] {
                Json::String(c) => c.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    fn rows(&self) -> Vec<Json> {
        let Ok(Json::Object(m)) = json::parse(&self.body) else { panic!("{}", self.body) };
        match &m["rows"] {
            Json::Array(rows) => rows.clone(),
            _ => panic!("{}", self.body),
        }
    }
}

fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{password}").as_bytes()))
}

fn http(addr: SocketAddr, method: &str, path: &str, auth: Option<&str>, body: Option<&str>) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    let body = body.unwrap_or("");
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

fn query(addr: SocketAddr, auth: &str, q: &str) -> Reply {
    let mut body = String::from(r#"{"query":"#);
    json::write_str(&mut body, q);
    body.push('}');
    http(addr, "POST", "/query", Some(auth), Some(&body))
}

#[test]
fn http_logins_and_roles() {
    let server = start(users(), Some("tok"));
    let addr = server.local_addr();
    let (root, ed, rita, nobody) =
        (basic("root", "rootpw"), basic("ed", "edpw"), basic("rita", "ritapw"), basic("nobody", "nopw"));

    // Logging in.
    let r = http(addr, "GET", "/db", None, None);
    assert_eq!(r.status, 401);
    let challenge = r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("www-authenticate")).unwrap();
    assert!(challenge.1.starts_with("Basic"));
    assert_eq!(http(addr, "GET", "/db", Some(&basic("root", "wrong")), None).status, 401);
    assert_eq!(http(addr, "GET", "/db", Some(&basic("ghost", "rootpw")), None).status, 401);
    assert_eq!(http(addr, "GET", "/db", Some(&root), None).status, 200);
    assert_eq!(http(addr, "GET", "/db", Some("Bearer tok"), None).status, 200);
    assert_eq!(http(addr, "GET", "/db", Some(&basic("anyone", "tok")), None).status, 200);
    assert_eq!(http(addr, "GET", "/health", None, None).status, 200);

    // Roles.
    assert_eq!(query(addr, &ed, "CREATE (:P {n: 1})").status, 200);
    assert_eq!(query(addr, &ed, "CREATE INDEX FOR (p:P) ON (p.n)").status, 200);
    let r = query(addr, &rita, "CREATE (:P {n: 2})");
    assert_eq!((r.status, r.code()), (403, "Forbidden".to_string()), "{}", r.body);
    assert_eq!(query(addr, &rita, "MATCH (p:P) RETURN p.n").rows().len(), 1);
    assert_eq!(query(addr, &rita, "EXPLAIN CREATE (:P)").status, 200);
    assert_eq!(query(addr, &rita, "SHOW DATABASES").status, 200);
    assert_eq!(query(addr, &rita, "CREATE DATABASE nope").status, 403);
    assert_eq!(query(addr, &ed, "CREATE DATABASE nope").status, 403);
    assert_eq!(query(addr, &root, "CREATE DATABASE yes").status, 200);
    assert_eq!(query(addr, &nobody, "RETURN 1").status, 403);
    assert_eq!(http(addr, "GET", "/db", Some(&nobody), None).status, 403);
    let me = query(addr, &nobody, "SHOW CURRENT USER");
    assert_eq!(me.status, 200, "{}", me.body);
    assert!(me.body.contains(r#""nobody""#));

    // Managing users.
    assert_eq!(query(addr, &ed, "CREATE USER x SET PASSWORD 'x'").status, 403);
    assert_eq!(query(addr, &root, "CREATE USER kim SET PASSWORD 'kimpw'").status, 200);
    let kim = basic("kim", "kimpw");
    assert_eq!(query(addr, &kim, "MATCH (n) RETURN n").status, 403);
    assert_eq!(query(addr, "Bearer tok", "GRANT ROLE reader TO kim").status, 200);
    assert_eq!(query(addr, &kim, "MATCH (n) RETURN n").status, 200);
    let users = query(addr, &root, "SHOW USERS");
    assert_eq!(users.rows().len(), 5, "{}", users.body);
    assert_eq!(
        query(addr, &kim, "ALTER CURRENT USER SET PASSWORD FROM 'kimpw' TO 'new'").status,
        200
    );
    assert_eq!(query(addr, &kim, "RETURN 1").status, 401);
    assert_eq!(query(addr, &basic("kim", "new"), "RETURN 1").status, 200);
    assert_eq!(query(addr, &root, "DROP USER kim").status, 200);
    assert_eq!(query(addr, &basic("kim", "new"), "RETURN 1").status, 401);
    let r = query(addr, &root, "DROP USER root");
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn http_transactions_check_roles_and_owners() {
    let server = start(users(), None);
    let addr = server.local_addr();
    let (ed, rita) = (basic("ed", "edpw"), basic("rita", "ritapw"));

    let begin = http(addr, "POST", "/tx", Some(&rita), Some(""));
    assert_eq!(begin.status, 201, "{}", begin.body);
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    let run = |auth: &str, q: &str| http(addr, "POST", &path, Some(auth), Some(&format!(r#"{{"query":"{q}"}}"#)));
    // A reader's transaction refuses writes but stays open.
    assert_eq!(run(&rita, "CREATE (n)").status, 403);
    // Calling a write procedure is writing.
    let set = "MATCH (n) CALL db.create.setNodeVectorProperty(n, 'v', [1])";
    assert_eq!(run(&rita, set).status, 403);
    assert_eq!(query(addr, &rita, set).status, 403);
    assert_eq!(run(&rita, "MATCH (n) RETURN count(n)").status, 200);
    // Another user cannot use it.
    assert_eq!(run(&ed, "MATCH (n) RETURN count(n)").status, 404);
    assert_eq!(http(addr, "POST", &format!("{path}/commit"), Some(&ed), Some("")).status, 404);
    assert_eq!(http(addr, "POST", &format!("{path}/commit"), Some(&rita), Some("")).status, 200);
    // User commands are not for transactions.
    let begin = http(addr, "POST", "/tx", Some(&basic("root", "rootpw")), Some(""));
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    let r = http(addr, "POST", &path, Some(&basic("root", "rootpw")), Some(r#"{"query":"SHOW USERS"}"#));
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(http(addr, "DELETE", &path, Some(&basic("root", "rootpw")), None).status, 200);
    assert_eq!(query(addr, &ed, set).status, 200);
}

#[test]
fn backup_restore_and_check_need_the_admin_role() {
    let server = start(users(), None);
    let addr = server.local_addr();
    for path in ["/db/graphbuff/backup", "/db/graphbuff/restore", "/db/graphbuff/check"] {
        for who in [basic("ed", "edpw"), basic("rita", "ritapw"), basic("nobody", "nopw")] {
            let r = http(addr, "POST", path, Some(&who), Some(r#"{"backup":"x"}"#));
            assert_eq!((r.status, r.code().as_str()), (403, "Forbidden"), "{path}: {}", r.body);
        }
        // An admin gets past the role check (backups are off on this server).
        let r = http(addr, "POST", path, Some(&basic("root", "rootpw")), Some(r#"{"backup":"x"}"#));
        assert_eq!((r.status, r.code().as_str()), (400, "Unsupported"), "{path}: {}", r.body);
    }
}

#[test]
fn users_without_accounts_and_in_the_system_database() {
    // A server without accounts (embedded): user commands say so.
    let server = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let r = http(
        server.local_addr(),
        "POST",
        "/query",
        None,
        Some(r#"{"query":"CREATE USER a SET PASSWORD 'b'"}"#),
    );
    assert_eq!(r.status, 400);
    assert!(r.body.contains("no user accounts"), "{}", r.body);
    drop(server);

    // Accounts created over the wire are in the system database afterwards.
    let dir: PathBuf = std::env::temp_dir().join(format!("graphbuff-users-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mgr = Arc::new(DatabaseManager::open(&dir).unwrap());
        let u = Users::system(mgr.system_database().unwrap()).unwrap().with_hash_params(HashParams::TESTING).with_min_password_length(1);
        u.create("root", "pw", &[Role::Admin], ExistingDatabase::Fail).unwrap();
        let server = Server::start(
            Arc::clone(&mgr),
            ServerConfig {
                bind: "127.0.0.1:0".into(),
                users: Some(Arc::new(u)),
                ..ServerConfig::default()
            },
        )
        .unwrap();
        let r = query(server.local_addr(), &basic("root", "pw"), "CREATE USER zed SET PASSWORD 'zz'");
        assert_eq!(r.status, 200, "{}", r.body);
        // The system database cannot be queried, even by an admin.
        let r = query(server.local_addr(), &basic("root", "pw"), "USE system MATCH (u:User) RETURN u.password");
        assert_ne!(r.status, 200, "{}", r.body);
    }
    let mgr = DatabaseManager::open(&dir).unwrap();
    let again = Users::system(mgr.system_database().unwrap()).unwrap();
    assert!(again.authenticate("zed", "zz"));
    drop(mgr);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Bolt ---------------------------------------------------------------------------------

const HELLO: u8 = 0x01;
const RUN: u8 = 0x10;
const BEGIN: u8 = 0x11;
const RESET: u8 = 0x0F;
const PULL: u8 = 0x3F;
const LOGON: u8 = 0x6A;
const LOGOFF: u8 = 0x6B;
const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const FAILURE: u8 = 0x7F;

struct Bolt(TcpStream);

fn s(v: &str) -> PackValue {
    PackValue::str(v)
}

impl Bolt {
    fn connect(addr: SocketAddr) -> Bolt {
        let mut t = TcpStream::connect(addr).unwrap();
        t.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        t.write_all(&[0x60, 0x60, 0xB0, 0x17, 0, 4, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        let mut v = [0u8; 4];
        t.read_exact(&mut v).unwrap();
        let mut b = Bolt(t);
        b.send(HELLO, vec![PackValue::map([("user_agent", s("test/1.0"))])]);
        assert_eq!(b.recv().0, SUCCESS);
        b
    }

    fn logon(&mut self, user: &str, password: &str) -> (u8, Vec<PackValue>) {
        let auth = PackValue::map([("scheme", s("basic")), ("principal", s(user)), ("credentials", s(password))]);
        self.send(LOGON, vec![auth]);
        self.recv()
    }

    fn send(&mut self, tag: u8, fields: Vec<PackValue>) {
        let mut buf = Vec::new();
        packstream::encode(&mut buf, &PackValue::Struct { tag, fields });
        let mut out = (buf.len() as u16).to_be_bytes().to_vec();
        out.extend_from_slice(&buf);
        out.extend_from_slice(&[0, 0]);
        self.0.write_all(&out).unwrap();
    }

    fn recv(&mut self) -> (u8, Vec<PackValue>) {
        let mut message = Vec::new();
        loop {
            let mut size = [0u8; 2];
            self.0.read_exact(&mut size).unwrap();
            let n = u16::from_be_bytes(size) as usize;
            if n == 0 {
                break;
            }
            let start = message.len();
            message.resize(start + n, 0);
            self.0.read_exact(&mut message[start..]).unwrap();
        }
        match packstream::decode(&message).unwrap() {
            PackValue::Struct { tag, fields } => (tag, fields),
            other => panic!("{other:?}"),
        }
    }

    /// Runs `q` (on database `db`, if given) and pulls everything: the rows, or the
    /// failure's code.
    fn run(&mut self, q: &str, db: Option<&str>) -> Result<Vec<Vec<PackValue>>, String> {
        let extra = match db {
            Some(d) => PackValue::map([("db", s(d))]),
            None => PackValue::Map(BTreeMap::new()),
        };
        self.send(RUN, vec![s(q), PackValue::Map(BTreeMap::new()), extra]);
        let (tag, fields) = self.recv();
        if tag == FAILURE {
            self.send(RESET, vec![]);
            assert_eq!(self.recv().0, SUCCESS);
            let PackValue::Map(m) = &fields[0] else { panic!() };
            let PackValue::String(code) = &m["code"] else { panic!() };
            return Err(code.clone());
        }
        self.send(PULL, vec![PackValue::map([("n", PackValue::Int(-1))])]);
        let mut rows = Vec::new();
        loop {
            match self.recv() {
                (RECORD, mut f) => match f.remove(0) {
                    PackValue::List(row) => rows.push(row),
                    _ => panic!(),
                },
                (SUCCESS, _) => return Ok(rows),
                other => panic!("{other:?}"),
            }
        }
    }
}

const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";

#[test]
fn bolt_logins_and_roles() {
    let server = start(users(), Some("tok"));
    let addr = server.bolt_addr().unwrap();

    let mut b = Bolt::connect(addr);
    let (tag, fields) = b.logon("rita", "wrong");
    assert_eq!(tag, FAILURE, "{fields:?}");

    let mut b = Bolt::connect(addr);
    assert_eq!(b.logon("rita", "ritapw").0, SUCCESS);
    assert_eq!(b.run("RETURN 1", None).unwrap().len(), 1);
    assert_eq!(b.run("CREATE (n)", None).unwrap_err(), FORBIDDEN);
    assert_eq!(b.run("SHOW USERS", None).unwrap_err(), FORBIDDEN);
    let me = b.run("SHOW CURRENT USER", Some("system")).unwrap();
    assert_eq!(me[0][0], s("rita"));
    // A reader may open a transaction, but not write in it.
    b.send(BEGIN, vec![PackValue::Map(BTreeMap::new())]);
    assert_eq!(b.recv().0, SUCCESS);
    b.send(RUN, vec![s("CREATE (n)"), PackValue::Map(BTreeMap::new()), PackValue::Map(BTreeMap::new())]);
    assert_eq!(b.recv().0, FAILURE);
    b.send(RESET, vec![]);
    assert_eq!(b.recv().0, SUCCESS);

    // Logging off and on as someone else changes what the connection may do.
    b.send(LOGOFF, vec![]);
    assert_eq!(b.recv().0, SUCCESS);
    assert_eq!(b.logon("root", "rootpw").0, SUCCESS);
    b.run("CREATE USER bo SET PASSWORD 'bopw'", Some("system")).unwrap();
    b.run("GRANT ROLE editor TO bo", None).unwrap();
    let users = b.run("SHOW USERS", Some("system")).unwrap();
    assert_eq!(users.len(), 5);

    let mut bo = Bolt::connect(addr);
    assert_eq!(bo.logon("bo", "bopw").0, SUCCESS);
    bo.run("CREATE (:Made)", None).unwrap();
    // A role taken away applies to open connections at once.
    b.run("REVOKE ROLE editor FROM bo", None).unwrap();
    assert_eq!(bo.run("CREATE (:Made)", None).unwrap_err(), FORBIDDEN);
    // So does a dropped account.
    b.run("DROP USER bo", None).unwrap();
    assert_eq!(bo.run("RETURN 1", None).unwrap_err(), "Neo.ClientError.Security.Unauthorized");

    // The token still works, with any user name.
    let mut t = Bolt::connect(addr);
    assert_eq!(t.logon("whoever", "tok").0, SUCCESS);
    assert_eq!(t.run("MATCH (m:Made) RETURN count(m)", None).unwrap()[0][0], PackValue::Int(1));
}

const RATE_LIMIT: &str = "Neo.ClientError.Security.AuthenticationRateLimit";

/// The failure code of a Bolt reply.
fn failure_code(fields: &[PackValue]) -> String {
    let PackValue::Map(m) = &fields[0] else { panic!("{fields:?}") };
    let PackValue::String(code) = &m["code"] else { panic!("{fields:?}") };
    code.clone()
}

#[test]
fn failed_logins_lock_the_client_out() {
    let server = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            bolt: Some("127.0.0.1:0".into()),
            token: Some("tok".into()),
            users: Some(users()),
            login_limits: LoginLimits { max_failures: 3, lockout: Duration::from_secs(60) },
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let (addr, bolt) = (server.local_addr(), server.bolt_addr().unwrap());
    let root = basic("root", "rootpw");

    // No credentials at all is not a guess.
    for _ in 0..5 {
        assert_eq!(http(addr, "GET", "/db", None, None).status, 401);
    }
    // A success clears the count.
    assert_eq!(http(addr, "GET", "/db", Some(&basic("root", "wrong")), None).status, 401);
    assert_eq!(http(addr, "GET", "/db", Some(&basic("root", "wrong")), None).status, 401);
    assert_eq!(http(addr, "GET", "/db", Some(&root), None).status, 200);
    // Failures over HTTP and Bolt count together: the third locks `root` out.
    assert_eq!(http(addr, "GET", "/db", Some(&basic("root", "wrong")), None).status, 401);
    let mut b = Bolt::connect(bolt);
    assert_eq!(failure_code(&b.logon("root", "wrong").1), "Neo.ClientError.Security.Unauthorized");
    assert_eq!(http(addr, "GET", "/db", Some(&basic("root", "wrong")), None).status, 401);

    // Now even the right password is refused, without being checked.
    let r = http(addr, "GET", "/db", Some(&root), None);
    assert_eq!((r.status, r.code()), (429, "TooManyRequests".to_string()), "{}", r.body);
    let retry = r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("retry-after")).unwrap();
    let secs: u64 = retry.1.parse().unwrap();
    assert!((59..=60).contains(&secs), "{secs}");
    let mut b = Bolt::connect(bolt);
    let (tag, fields) = b.logon("root", "rootpw");
    assert_eq!((tag, failure_code(&fields)), (FAILURE, RATE_LIMIT.to_string()));

    // Other user names from this address still log in, until their failures add up.
    assert_eq!(http(addr, "GET", "/db", Some(&basic("rita", "ritapw")), None).status, 200);
    assert_eq!(http(addr, "GET", "/db", Some("Bearer tok"), None).status, 200);
    // (Five failures so far from this address: the address's own count is not cleared
    // by a success.)
    for name in ["ed", "rita", "nobody"] {
        assert_eq!(http(addr, "GET", "/db", Some(&basic(name, "wrong")), None).status, 401, "{name}");
    }
    assert_eq!(http(addr, "GET", "/db", Some(&basic("rita", "ritapw")), None).status, 200);
    // The ninth failure from this address over all names locks it out, token included.
    assert_eq!(http(addr, "GET", "/db", Some(&basic("ghost", "wrong")), None).status, 401);
    assert_eq!(http(addr, "GET", "/db", Some("Bearer tok"), None).status, 429);
    assert_eq!(http(addr, "GET", "/db", Some(&basic("rita", "ritapw")), None).status, 429);
    // The health check needs no login.
    assert_eq!(http(addr, "GET", "/health", None, None).status, 200);
}

// ---- SHOW / TERMINATE TRANSACTIONS ----------------------------------------------------------

/// A query that runs until it is stopped.
const ENDLESS: &str = "UNWIND range(1, 1000000) AS i UNWIND range(1, 1000000) AS endless WITH endless WHERE endless = -1 RETURN count(*)";

/// The rows of `q` as root, as compact JSON.
fn rows_of(addr: SocketAddr, auth: &str, q: &str) -> Vec<String> {
    let r = query(addr, auth, q);
    assert_eq!(r.status, 200, "{q}: {}", r.body);
    r.rows().iter().map(enc).collect()
}

/// Waits until `q` (as `auth`) returns a row, and returns the rows.
fn wait_for(addr: SocketAddr, auth: &str, q: &str) -> Vec<String> {
    for _ in 0..200 {
        let rows = rows_of(addr, auth, q);
        if !rows.is_empty() {
            return rows;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("no rows for {q}");
}

fn first_string(row: &str) -> String {
    match json::parse(row).unwrap() {
        Json::Array(v) => match &v[0] {
            Json::String(s) => s.clone(),
            o => panic!("{o:?}"),
        },
        o => panic!("{o:?}"),
    }
}

#[test]
fn show_and_terminate_transactions_over_http() {
    let server = start(users(), None);
    let addr = server.local_addr();
    let (root, ed) = (basic("root", "rootpw"), basic("ed", "edpw"));

    // A query run on its own, by ed.
    let runner = {
        let ed = ed.clone();
        std::thread::spawn(move || query(addr, &ed, ENDLESS))
    };
    let find = "SHOW TRANSACTIONS YIELD transactionId, currentQuery, username, status, protocol
                WHERE currentQuery CONTAINS 'AS endless'
                RETURN transactionId, username, status, protocol";
    let rows = wait_for(addr, &root, find);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].ends_with(r#","ed","Running","http"]"#), "{rows:?}");
    let id = first_string(&rows[0]);
    assert!(id.starts_with("graphbuff-transaction-"), "{id}");
    // The default columns.
    let r = query(addr, &root, "SHOW TRANSACTIONS");
    let Ok(Json::Object(m)) = json::parse(&r.body) else { panic!() };
    let Json::Array(columns) = &m["columns"] else { panic!() };
    let names: Vec<String> = columns.iter().map(enc).collect();
    assert_eq!(
        names.join(","),
        r#""database","transactionId","currentQueryId","connectionId","clientAddress","username","currentQuery","startTime","status","elapsedTime""#
    );

    // Others see only their own; rita cannot terminate it.
    let rita = basic("rita", "ritapw");
    assert!(rows_of(addr, &rita, find).is_empty());
    let r = rows_of(addr, &rita, &format!("TERMINATE TRANSACTION '{id}'"));
    assert_eq!(r, [format!(r#"["{id}",null,"Transaction not found."]"#)]);
    // ed sees and terminates their own.
    assert_eq!(rows_of(addr, &ed, find).len(), 1);
    let r = rows_of(addr, &ed, &format!("TERMINATE TRANSACTIONS '{id}', 'nope'"));
    assert_eq!(r, [format!(r#"["{id}","ed","Transaction terminated."]"#), r#"["nope",null,"Transaction not found."]"#.into()]);
    let stopped = runner.join().unwrap();
    assert_eq!((stopped.status, stopped.code()), (409, "Cancelled".to_string()), "{}", stopped.body);
    assert!(rows_of(addr, &root, find).is_empty());

    // An explicit transaction: listed while open, idle between statements.
    let begin = http(addr, "POST", "/tx", Some(&ed), Some(""));
    assert_eq!(begin.status, 201, "{}", begin.body);
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    let run = |q: &str| http(addr, "POST", &path, Some(&ed), Some(&format!(r#"{{"query":"{q}"}}"#)));
    assert_eq!(run("CREATE (:Doomed)").status, 200);
    let idle = rows_of(addr, &root, "SHOW TRANSACTIONS YIELD transactionId, status, username WHERE status = 'Idle'");
    assert_eq!(idle.len(), 1, "{idle:?}");
    assert!(idle[0].ends_with(r#","Idle","ed"]"#), "{idle:?}");
    let id = first_string(&idle[0]);
    let r = rows_of(addr, &root, &format!("TERMINATE TRANSACTION '{id}' YIELD message"));
    assert_eq!(r, [r#"["Transaction terminated."]"#]);
    // Rolled back: gone, its changes undone, and the database is free for other writers.
    assert_eq!(run("RETURN 1").status, 404);
    assert_eq!(rows_of(addr, &root, "MATCH (d:Doomed) RETURN count(d)"), ["[0]"]);
    assert_eq!(query(addr, &root, "CREATE (:After)").status, 200);

    // Ids by parameter; nothing listed for unknown ids.
    let body = r#"{"query": "SHOW TRANSACTIONS $ids", "params": {"ids": ["x", "y"]}}"#;
    let r = http(addr, "POST", "/query", Some(&root), Some(body));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.rows().is_empty());
    // Not inside a transaction.
    let begin = http(addr, "POST", "/tx", Some(&root), Some(""));
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    let r = http(addr, "POST", &path, Some(&root), Some(r#"{"query":"SHOW TRANSACTIONS"}"#));
    assert_eq!(r.status, 400, "{}", r.body);
}

#[test]
fn terminate_bolt_transactions() {
    let server = start(users(), None);
    let (http_addr, bolt) = (server.local_addr(), server.bolt_addr().unwrap());
    let root = basic("root", "rootpw");

    // An explicit Bolt transaction holding the write lock, idle.
    let mut b = Bolt::connect(bolt);
    assert_eq!(b.logon("ed", "edpw").0, SUCCESS);
    b.send(BEGIN, vec![PackValue::Map(BTreeMap::new())]);
    assert_eq!(b.recv().0, SUCCESS);
    b.run("CREATE (:Held)", None).unwrap();
    let rows = rows_of(
        http_addr,
        &root,
        "SHOW TRANSACTIONS YIELD transactionId, protocol, connectionId, status WHERE protocol = 'bolt' RETURN transactionId, connectionId, status",
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].contains(r#""bolt-"#) && rows[0].ends_with(r#","Idle"]"#), "{rows:?}");
    let id = first_string(&rows[0]);

    // Terminated over Bolt by an administrator.
    let mut admin = Bolt::connect(bolt);
    assert_eq!(admin.logon("root", "rootpw").0, SUCCESS);
    let r = admin.run(&format!("TERMINATE TRANSACTION '{id}'"), None).unwrap();
    assert_eq!(r[0][2], PackValue::str("Transaction terminated."));
    // Its connection rolls it back without waiting for a message: other writers go on.
    assert_eq!(query(http_addr, &root, "CREATE (:Other)").status, 200);
    assert_eq!(rows_of(http_addr, &root, "MATCH (h:Held) RETURN count(h)"), ["[0]"]);
    // The client learns at its next message.
    b.send(RUN, vec![PackValue::str("RETURN 1"), PackValue::Map(BTreeMap::new()), PackValue::Map(BTreeMap::new())]);
    let (tag, fields) = b.recv();
    assert_eq!(tag, FAILURE, "{fields:?}");
    let PackValue::Map(m) = &fields[0] else { panic!() };
    assert_eq!(m["code"], PackValue::str("Neo.ClientError.Transaction.Terminated"));

    // A Bolt query run on its own, terminated while it runs.
    let runner = std::thread::spawn(move || {
        let mut c = Bolt::connect(bolt);
        assert_eq!(c.logon("ed", "edpw").0, SUCCESS);
        c.run(ENDLESS, None)
    });
    let find = "SHOW TRANSACTIONS YIELD transactionId, currentQuery WHERE currentQuery CONTAINS 'AS endless' RETURN transactionId";
    let id = first_string(&wait_for(http_addr, &root, find)[0]);
    assert_eq!(
        rows_of(http_addr, &root, &format!("TERMINATE TRANSACTION '{id}' YIELD message")),
        [r#"["Transaction terminated."]"#]
    );
    assert_eq!(runner.join().unwrap().unwrap_err(), "Neo.ClientError.Transaction.Terminated");
}

/// Compact JSON for a parsed value (strings unescaped: only test data).
fn enc(j: &Json) -> String {
    match j {
        Json::Null => "null".into(),
        Json::Bool(b) => b.to_string(),
        Json::Int(i) => i.to_string(),
        Json::Float(f) => f.to_string(),
        Json::String(s) => format!("\"{s}\""),
        Json::Array(v) => format!("[{}]", v.iter().map(enc).collect::<Vec<_>>().join(",")),
        Json::Object(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("\"{k}\":{}", enc(v))).collect::<Vec<_>>().join(",")),
    }
}

// ---- metrics ----------------------------------------------------------------------------------

/// The samples of a Prometheus text body: `name{labels}` → value.
fn samples(body: &str) -> BTreeMap<String, f64> {
    body.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let (k, v) = l.rsplit_once(' ').unwrap();
            (k.to_string(), v.parse().unwrap())
        })
        .collect()
}

#[test]
fn metrics_count_queries_logins_and_connections() {
    let server = start(users(), Some("tok"));
    let (addr, bolt) = (server.local_addr(), server.bolt_addr().unwrap());
    let (root, ed) = (basic("root", "rootpw"), basic("ed", "edpw"));

    // Administrators and the token only.
    assert_eq!(http(addr, "GET", "/metrics", None, None).status, 401);
    let r = http(addr, "GET", "/metrics", Some(&basic("rita", "ritapw")), None);
    assert_eq!((r.status, r.code()), (403, "Forbidden".to_string()), "{}", r.body);
    assert_eq!(http(addr, "POST", "/metrics", Some(&root), Some("")).status, 405);

    assert_eq!(query(addr, &ed, "CREATE (:A), (:A)-[:R]->(:B)").status, 200);
    assert_eq!(query(addr, &ed, "RETURN 1").status, 200);
    assert_eq!(query(addr, &ed, "RETURN 1 +").status, 400);
    assert_eq!(http(addr, "GET", "/db", Some(&basic("ed", "wrong")), None).status, 401);
    let mut b = Bolt::connect(bolt);
    assert_eq!(b.logon("ed", "edpw").0, SUCCESS);
    b.run("MATCH (n) RETURN count(n)", None).unwrap();

    let r = http(addr, "GET", "/metrics", Some("Bearer tok"), None);
    assert_eq!(r.status, 200, "{}", r.body);
    let ct = r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).unwrap();
    assert_eq!(ct.1, "text/plain; version=0.0.4; charset=utf-8");
    assert!(r.body.contains("# TYPE graphbuff_query_duration_seconds histogram\n"), "{}", r.body);
    let m = samples(&r.body);
    let get = |k: &str| *m.get(k).unwrap_or_else(|| panic!("no {k} in\n{}", r.body));
    assert_eq!(get(r#"graphbuff_queries_total{protocol="http",outcome="ok"}"#), 2.0);
    assert_eq!(get(r#"graphbuff_queries_total{protocol="http",outcome="error"}"#), 1.0);
    assert_eq!(get(r#"graphbuff_queries_total{protocol="bolt",outcome="ok"}"#), 1.0);
    assert_eq!(get(r#"graphbuff_queries_total{protocol="bolt",outcome="timeout"}"#), 0.0);
    assert_eq!(get(r#"graphbuff_query_duration_seconds_count{protocol="http"}"#), 3.0);
    assert_eq!(get(r#"graphbuff_query_duration_seconds_bucket{protocol="http",le="+Inf"}"#), 3.0);
    assert!(get(r#"graphbuff_query_duration_seconds_bucket{protocol="http",le="10"}"#) <= 3.0);
    // The request without credentials and the wrong password.
    assert_eq!(get(r#"graphbuff_failed_logins_total{protocol="http"}"#), 2.0);
    assert_eq!(get(r#"graphbuff_login_lockouts_total"#), 0.0);
    assert_eq!(get(r#"graphbuff_connections{protocol="bolt"}"#), 1.0);
    // This scrape's own connection is open.
    assert!(get(r#"graphbuff_connections{protocol="http"}"#) >= 1.0);
    assert!(get(r#"graphbuff_connections_accepted_total{protocol="http"}"#) >= 7.0);
    assert_eq!(get(r#"graphbuff_database_nodes{database="graphbuff"}"#), 3.0);
    assert_eq!(get(r#"graphbuff_database_relationships{database="graphbuff"}"#), 1.0);
    assert_eq!(get(r#"graphbuff_database_cached_indexes{database="graphbuff"}"#), 0.0);
    assert_eq!(get(r#"graphbuff_transactions{protocol="http",status="idle"}"#), 0.0);
    assert!(m.keys().any(|k| k.starts_with("graphbuff_info{version=")));
    assert!(!r.body.contains("graphbuff_database_store_bytes{"), "in-memory databases have no files");

    // An open transaction shows, idle between statements.
    b.send(BEGIN, vec![PackValue::Map(BTreeMap::new())]);
    assert_eq!(b.recv().0, SUCCESS);
    let m = samples(&http(addr, "GET", "/metrics", Some(&root), None).body);
    assert_eq!(m[r#"graphbuff_transactions{protocol="bolt",status="idle"}"#], 1.0);
    drop(b);
}

#[test]
fn metrics_show_store_size() {
    let dir = std::env::temp_dir().join(format!("graphbuff-metrics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = Server::start(
        Arc::new(DatabaseManager::open(&dir).unwrap()),
        ServerConfig { bind: "127.0.0.1:0".into(), token: Some("tok".into()), ..ServerConfig::default() },
    )
    .unwrap();
    let addr = server.local_addr();
    assert_eq!(query(addr, "Bearer tok", "UNWIND range(1, 100) AS i CREATE (:N {i: i})").status, 200);
    let m = samples(&http(addr, "GET", "/metrics", Some("Bearer tok"), None).body);
    assert!(m[r#"graphbuff_database_store_bytes{database="graphbuff"}"#] > 0.0, "{m:?}");
    assert_eq!(m[r#"graphbuff_database_nodes{database="graphbuff"}"#], 100.0);
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- roles on one database --------------------------------------------------------------------

#[test]
fn roles_on_one_database() {
    let server = start(users(), None);
    let (addr, bolt) = (server.local_addr(), server.bolt_addr().unwrap());
    let root = basic("root", "rootpw");
    let ok = |auth: &str, q: &str| {
        let r = query(addr, auth, q);
        assert_eq!(r.status, 200, "{q}: {}", r.body);
    };
    ok(&root, "CREATE DATABASE movies");
    ok(&root, "CREATE DATABASE other");
    ok(&root, "CREATE USER m SET PASSWORD 'mpw'");
    ok(&root, "GRANT ROLE editor ON DATABASE Movies TO m");
    ok(&root, "GRANT ROLES reader ON DATABASE other TO m");
    let refused = query(addr, &root, "GRANT ROLE admin ON DATABASE movies TO m");
    assert_eq!(refused.status, 400, "{}", refused.body);
    assert!(refused.body.contains("without ON DATABASE"), "{}", refused.body);
    let missing = query(addr, &root, "GRANT ROLE reader ON DATABASE nope TO m");
    assert_eq!(missing.status, 400, "{}", missing.body);
    assert!(missing.body.contains("does not exist"), "{}", missing.body);
    // Only administrators grant.
    assert_eq!(query(addr, &basic("ed", "edpw"), "GRANT ROLE editor ON DATABASE movies TO ed").status, 403);

    let m = basic("m", "mpw");
    let on = |db: &str, q: &str| {
        let mut body = String::from(r#"{"query":"#);
        json::write_str(&mut body, q);
        body.push('}');
        http(addr, "POST", &format!("/db/{db}/query"), Some(&m), Some(&body)).status
    };
    assert_eq!(on("movies", "CREATE (:Movie)"), 200);
    assert_eq!(on("other", "MATCH (n) RETURN count(n)"), 200);
    assert_eq!(on("other", "CREATE (:X)"), 403);
    // Nothing on the default database; USE picks the database that counts.
    assert_eq!(query(addr, &m, "MATCH (n) RETURN count(n)").status, 403);
    assert_eq!(query(addr, &m, "USE movies CREATE (:Movie)").status, 200);
    assert_eq!(on("movies", "USE graphbuff MATCH (n) RETURN n"), 403);
    // Listing databases needs a role somewhere.
    assert_eq!(http(addr, "GET", "/db", Some(&m), None).status, 200);
    assert_eq!(query(addr, &m, "SHOW DATABASES").status, 200);
    assert_eq!(http(addr, "GET", "/db", Some(&basic("nobody", "nopw")), None).status, 403);
    // Database commands and user management stay with administrators.
    assert_eq!(query(addr, &m, "CREATE DATABASE mine").status, 403);
    assert_eq!(query(addr, &m, "SHOW USERS").status, 403);

    // Explicit transactions.
    let begin = http(addr, "POST", "/db/movies/tx", Some(&m), Some(""));
    assert_eq!(begin.status, 201, "{}", begin.body);
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    assert_eq!(http(addr, "POST", &path, Some(&m), Some(r#"{"query":"CREATE (:Movie)"}"#)).status, 200);
    assert_eq!(http(addr, "POST", &format!("{path}/commit"), Some(&m), Some("")).status, 200);
    assert_eq!(http(addr, "POST", "/db/graphbuff/tx", Some(&m), Some("")).status, 403);
    let begin = http(addr, "POST", "/db/other/tx", Some(&m), Some(""));
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    assert_eq!(http(addr, "POST", &path, Some(&m), Some(r#"{"query":"CREATE (:X)"}"#)).status, 403);
    // A refused statement leaves the transaction open; it holds the database until rolled back.
    assert_eq!(http(addr, "DELETE", &path, Some(&m), None).status, 200);

    // Bolt.
    let mut b = Bolt::connect(bolt);
    assert_eq!(b.logon("m", "mpw").0, SUCCESS);
    assert_eq!(b.run("MATCH (n:Movie) RETURN count(n)", Some("movies")).unwrap(), vec![vec![PackValue::Int(3)]]);
    assert_eq!(b.run("MATCH (n) RETURN n", None).unwrap_err(), FORBIDDEN);
    assert_eq!(b.run("CREATE (:X)", Some("other")).unwrap_err(), FORBIDDEN);
    b.send(BEGIN, vec![PackValue::map([("db", s("other"))])]);
    assert_eq!(b.recv().0, SUCCESS);
    b.send(RUN, vec![s("CREATE (:X)"), PackValue::Map(BTreeMap::new()), PackValue::Map(BTreeMap::new())]);
    let (tag, fields) = b.recv();
    assert_eq!((tag, failure_code(&fields)), (FAILURE, FORBIDDEN.to_string()));

    // Listed by SHOW USERS; revoking takes the access away.
    let users = query(addr, &root, "SHOW USERS");
    let row = users.rows().iter().map(enc).find(|r| r.starts_with(r#"["m","#)).unwrap();
    assert_eq!(row, r#"["m",[],false,false,null,{"movies":["editor"],"other":["reader"]}]"#);
    ok(&root, "REVOKE ROLE editor ON DATABASE movies FROM m");
    assert_eq!(on("movies", "CREATE (:Movie)"), 403);
    // A role held everywhere still counts on every database.
    ok(&root, "GRANT ROLE reader TO m");
    assert_eq!(query(addr, &m, "MATCH (n) RETURN count(n)").status, 200);
}

// ---- API tokens -----------------------------------------------------------------------------

/// Makes an API token with `q` as `auth` and returns its `Authorization` header.
fn make_token(addr: SocketAddr, auth: &str, q: &str) -> String {
    let r = query(addr, auth, q);
    assert_eq!(r.status, 200, "{q}: {}", r.body);
    let Json::Array(row) = r.rows().remove(0) else { panic!("{}", r.body) };
    let Some(Json::String(token)) = row.last() else { panic!("{}", r.body) };
    assert!(token.starts_with("gbt_"), "{token}");
    format!("Bearer {token}")
}

#[test]
fn api_tokens_over_http() {
    let server = start(users(), Some("tok"));
    let addr = server.local_addr();
    let (root, ed, rita, nobody) =
        (basic("root", "rootpw"), basic("ed", "edpw"), basic("rita", "ritapw"), basic("nobody", "nopw"));
    for q in ["CREATE DATABASE movies", "CREATE DATABASE other", "CREATE ALIAS films FOR DATABASE movies"] {
        assert_eq!(query(addr, &root, q).status, 200, "{q}");
    }

    let admin_write = make_token(addr, &root, "CREATE TOKEN w ACCESS WRITE");
    let admin_read = make_token(addr, &root, "CREATE TOKEN r");
    let ed_write = make_token(addr, &ed, "CREATE TOKEN w ACCESS WRITE");
    let ed_read = make_token(addr, &ed, "CREATE TOKEN r ACCESS READ EXPIRES NEVER");
    let rita_write = make_token(addr, &rita, "CREATE TOKEN w ACCESS WRITE");
    let nobody_write = make_token(addr, &nobody, "CREATE TOKEN w ACCESS WRITE");
    let movies = make_token(addr, &root, "CREATE TOKEN m ACCESS WRITE ON DATABASE Films");
    let given = make_token(addr, "Bearer tok", "CREATE TOKEN given FOR USER ed ACCESS WRITE");
    assert_eq!(query(addr, &root, "CREATE TOKEN x ON DATABASE nope").status, 400);
    // Without a role, and for tokens, the answer does not tell which databases exist.
    for auth in [&nobody, &admin_write] {
        for db in ["movies", "nope"] {
            assert_eq!(query(addr, auth, &format!("CREATE TOKEN x ON DATABASE {db}")).status, 403, "{db}");
        }
    }

    // What each may do: (statement, its status with each token).
    let all = [&admin_write, &admin_read, &ed_write, &ed_read, &rita_write, &nobody_write, &movies, &given];
    let cases: [(&str, [u16; 8]); 20] = [
        ("MATCH (n) RETURN count(n)", [200, 200, 200, 200, 200, 403, 403, 200]),
        ("EXPLAIN CREATE (:P)", [200, 200, 200, 200, 200, 403, 403, 200]),
        ("PROFILE CREATE (:P)", [200, 403, 200, 403, 403, 403, 403, 200]),
        ("CREATE (:P {n: 1})", [200, 403, 200, 403, 403, 403, 403, 200]),
        ("MATCH (p:P) CALL db.create.setNodeVectorProperty(p, 'v', [1.0])", [200, 403, 200, 403, 403, 403, 403, 200]),
        ("CREATE INDEX i IF NOT EXISTS FOR (p:P) ON (p.n)", [200, 403, 200, 403, 403, 403, 403, 200]),
        ("USE movies MATCH (n) RETURN count(n)", [200, 200, 200, 200, 200, 403, 200, 200]),
        ("USE films CREATE (:M)", [200, 403, 200, 403, 403, 403, 200, 200]),
        ("USE other CREATE (:M)", [200, 403, 200, 403, 403, 403, 403, 200]),
        ("SHOW DATABASES", [200, 200, 200, 200, 200, 403, 200, 200]),
        ("CREATE DATABASE nope", [403; 8]),
        ("DROP DATABASE other", [403; 8]),
        ("ALTER DATABASE other SET ACCESS READ ONLY", [403; 8]),
        ("SHOW USERS", [403; 8]),
        ("CREATE USER evil SET PASSWORD 'evilpw'", [403; 8]),
        ("GRANT ROLE admin TO nobody", [403; 8]),
        ("CREATE TOKEN more ACCESS WRITE EXPIRES NEVER", [403; 8]),
        ("SHOW TOKENS", [403; 8]),
        ("SHOW SETTINGS", [403; 8]),
        ("SHOW CURRENT USER", [200; 8]),
    ];
    for (q, want) in cases {
        let got: Vec<u16> = all.iter().map(|t| query(addr, t, q).status).collect();
        assert_eq!(got, want, "{q}");
    }
    for t in all {
        assert_eq!(query(addr, t, "ALTER CURRENT USER SET PASSWORD FROM 'rootpw' TO 'x'").status, 403);
        assert_eq!(http(addr, "GET", "/metrics", Some(t), None).status, 403);
        for path in ["/db/movies/backup", "/db/movies/restore", "/db/movies/check"] {
            assert_eq!(http(addr, "POST", path, Some(t), Some(r#"{"backup":"b"}"#)).status, 403, "{path}");
        }
    }
    assert!(query(addr, &root, "SHOW CURRENT USER").body.contains("root"));
    let refused = query(addr, &admin_read, "CREATE (:P)").body;
    assert!(refused.contains("editor role (or a stronger one), or an API token whose"), "{refused}");
    assert!(!query(addr, &rita, "CREATE (:P)").body.contains("API token"));

    // Explicit transactions belong to the token.
    assert_eq!(http(addr, "POST", "/db/other/tx", Some(&movies), Some("")).status, 403);
    let begin = http(addr, "POST", "/db/movies/tx", Some(&movies), Some(""));
    assert_eq!(begin.status, 201, "{}", begin.body);
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    let body = Some(r#"{"query":"CREATE (:M)"}"#);
    assert_eq!(http(addr, "POST", &path, Some(&movies), body).status, 200);
    for other in [&root, &admin_write, &ed_write] {
        assert_eq!(http(addr, "POST", &path, Some(other), body).status, 404);
    }
    assert_eq!(http(addr, "POST", &format!("{path}/commit"), Some(&movies), Some("")).status, 200);
    let begin = http(addr, "POST", "/db/movies/tx", Some(&admin_read), Some(""));
    let path = begin.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("location")).unwrap().1.clone();
    assert_eq!(http(addr, "POST", &path, Some(&admin_read), body).status, 403);
    assert_eq!(http(addr, "DELETE", &path, Some(&admin_read), None).status, 200);

    // Dropped, or its user gone: no login.
    assert_eq!(query(addr, &ed, "DROP TOKEN w").status, 200);
    assert_eq!(query(addr, &ed_write, "RETURN 1").status, 401);
    assert_eq!(query(addr, &root, "REVOKE ROLE editor FROM ed").status, 200);
    assert_eq!(query(addr, &given, "RETURN 1").status, 403);
    assert_eq!(query(addr, &root, "DROP USER ed").status, 200);
    assert_eq!(query(addr, &given, "RETURN 1").status, 401);
    // A token is not a password.
    let token = admin_write.strip_prefix("Bearer ").unwrap();
    assert_eq!(query(addr, &basic("root", token), "RETURN 1").status, 401);
}

#[test]
fn api_tokens_over_bolt() {
    let users = users();
    let (token, _) = users.create_token("root", "t", TokenAccess::Read, &[], None).unwrap();
    let (expired, _) = users.create_token("root", "old", TokenAccess::Read, &[], Some(Duration::ZERO)).unwrap();
    let server = start(Arc::clone(&users), None);
    let logon = |credentials: &str| {
        let mut b = Bolt::connect(server.bolt_addr().unwrap());
        let auth = PackValue::map([("scheme", s("bearer")), ("credentials", s(credentials))]);
        b.send(LOGON, vec![auth]);
        let reply = b.recv();
        (b, reply)
    };
    let (mut b, reply) = logon(&token);
    assert_eq!(reply.0, SUCCESS, "{reply:?}");
    assert_eq!(b.run("RETURN 1", None).unwrap().len(), 1);
    assert_eq!(b.run("CREATE (n)", None).unwrap_err(), FORBIDDEN);
    assert_eq!(b.run("SHOW USERS", Some("system")).unwrap_err(), FORBIDDEN);
    assert_eq!(b.run("CREATE TOKEN more", Some("system")).unwrap_err(), FORBIDDEN);
    assert_eq!(b.run("CREATE DATABASE nope", Some("system")).unwrap_err(), FORBIDDEN);
    b.send(BEGIN, vec![PackValue::Map(BTreeMap::new())]);
    assert_eq!(b.recv().0, SUCCESS);
    b.send(RUN, vec![s("CREATE (n)"), PackValue::Map(BTreeMap::new()), PackValue::Map(BTreeMap::new())]);
    let (tag, fields) = b.recv();
    assert_eq!((tag, failure_code(&fields)), (FAILURE, FORBIDDEN.to_string()));
    b.send(RESET, vec![]);
    assert_eq!(b.recv().0, SUCCESS);
    // Dropping it applies to open connections.
    users.drop_token("root", "t").unwrap();
    assert_eq!(b.run("RETURN 1", None).unwrap_err(), "Neo.ClientError.Security.Unauthorized");
    for bad in [&token, &expired] {
        let (_, (tag, fields)) = logon(bad);
        assert_eq!((tag, failure_code(&fields)), (FAILURE, "Neo.ClientError.Security.Unauthorized".to_string()));
    }
}

#[test]
fn expired_tokens_do_not_lock_out_and_wrong_ones_do() {
    let users = users();
    let (good, _) = users.create_token("rita", "t", TokenAccess::Read, &[], None).unwrap();
    let (expired, _) = users.create_token("rita", "old", TokenAccess::Read, &[], Some(Duration::ZERO)).unwrap();
    let server = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            users: Some(users),
            login_limits: LoginLimits { max_failures: 3, lockout: Duration::from_secs(60) },
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let addr = server.local_addr();
    let status = |token: &str| query(addr, &format!("Bearer {token}"), "RETURN 1").status;
    for _ in 0..20 {
        assert_eq!(status(&expired), 401);
    }
    assert_eq!(status(&good), 200);
    // Guessing the secret of one token locks out that token from this address, not others.
    let wrong = format!("{}{}", &good[..21], "0".repeat(64));
    for _ in 0..3 {
        assert_eq!(status(&wrong), 401);
    }
    assert_eq!(status(&good), 429);
    assert_eq!(status(&expired), 401);
    assert_eq!(query(addr, &basic("rita", "ritapw"), "RETURN 1").status, 200);
}

#[test]
fn users_see_the_queries_of_their_tokens() {
    let users = users();
    let (a, _) = users.create_token("rita", "a", TokenAccess::Read, &[], None).unwrap();
    let (b, _) = users.create_token("rita", "b", TokenAccess::Read, &[], None).unwrap();
    let server = start(users, None);
    let addr = server.local_addr();
    let (a, b) = (format!("Bearer {a}"), format!("Bearer {b}"));
    let runner = a.clone();
    let running = std::thread::spawn(move || query(addr, &runner, ENDLESS).status);
    let show = "SHOW TRANSACTIONS YIELD transactionId, username, currentQuery WHERE currentQuery CONTAINS 'endless'";
    // The user sees what the token runs, under their name; their other tokens do not.
    let rows = wait_for(addr, &basic("rita", "ritapw"), show);
    assert!(rows[0].contains(r#""rita""#), "{rows:?}");
    assert!(rows_of(addr, &b, show).is_empty());
    assert!(rows_of(addr, &basic("ed", "edpw"), show).is_empty());
    let id = first_string(&rows[0]);
    let ended = rows_of(addr, &b, &format!("TERMINATE TRANSACTIONS '{id}'"));
    assert!(ended[0].contains("not found"), "{ended:?}");
    let ended = rows_of(addr, &basic("rita", "ritapw"), &format!("TERMINATE TRANSACTIONS '{id}'"));
    assert!(ended[0].contains("terminated"), "{ended:?}");
    assert_eq!(running.join().unwrap(), 409);
}
