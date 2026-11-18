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
use graphbuff_server::{Role, Server, ServerConfig, Users};

fn users() -> Arc<Users> {
    let u = Users::in_memory().with_iterations(100);
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
    // Accounts off: user commands say so.
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
    assert!(r.body.contains("user accounts are off"), "{}", r.body);
    drop(server);

    // Accounts created over the wire are in the system database afterwards.
    let dir: PathBuf = std::env::temp_dir().join(format!("graphbuff-users-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mgr = Arc::new(DatabaseManager::open(&dir).unwrap());
        let u = Users::system(mgr.system_database().unwrap()).unwrap().with_iterations(100);
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
