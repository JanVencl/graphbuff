//! End-to-end tests of the Bolt protocol: a real server on a local port, spoken to by a
//! small client built on the server's PackStream codec.

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::DatabaseManager;
use graphbuff_server::packstream::{self, PackValue};
use graphbuff_server::{Server, ServerConfig};

const HELLO: u8 = 0x01;
const GOODBYE: u8 = 0x02;
const RESET: u8 = 0x0F;
const RUN: u8 = 0x10;
const BEGIN: u8 = 0x11;
const COMMIT: u8 = 0x12;
const ROLLBACK: u8 = 0x13;
const DISCARD: u8 = 0x2F;
const PULL: u8 = 0x3F;
const TELEMETRY: u8 = 0x54;
const ROUTE: u8 = 0x66;
const LOGON: u8 = 0x6A;
const LOGOFF: u8 = 0x6B;
const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7E;
const FAILURE: u8 = 0x7F;

const V5: [u8; 4] = [0, 4, 4, 5]; // 5.4 down to 5.0
const V44: [u8; 4] = [0, 0, 4, 4];
const NONE: [u8; 4] = [0; 4];

fn start(config: ServerConfig) -> Server {
    let mgr = Arc::new(DatabaseManager::in_memory());
    Server::start(
        mgr,
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            bolt: Some("127.0.0.1:0".into()),
            ..config
        },
    )
    .unwrap()
}

fn s(v: &str) -> PackValue {
    PackValue::str(v)
}

fn map(pairs: &[(&str, PackValue)]) -> PackValue {
    PackValue::map(pairs.iter().cloned())
}

fn empty() -> PackValue {
    PackValue::Map(BTreeMap::new())
}

fn get<'a>(m: &'a BTreeMap<String, PackValue>, k: &str) -> &'a PackValue {
    m.get(k).unwrap_or_else(|| panic!("no `{k}` in {m:?}"))
}

struct Bolt {
    s: TcpStream,
    version: [u8; 4],
}

impl Bolt {
    /// Connects and does the handshake; `version` is the server's answer.
    fn handshake(addr: SocketAddr, proposals: [[u8; 4]; 4]) -> Bolt {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(&[0x60, 0x60, 0xB0, 0x17]).unwrap();
        s.write_all(&proposals.concat()).unwrap();
        let mut version = [0u8; 4];
        s.read_exact(&mut version).unwrap();
        Bolt { s, version }
    }

    /// A Bolt 5.4 connection, authenticated (with `password`, if given).
    fn open(addr: SocketAddr) -> Bolt {
        Bolt::open_with(addr, None)
    }

    fn open_with(addr: SocketAddr, password: Option<&str>) -> Bolt {
        let mut b = Bolt::handshake(addr, [V5, V44, NONE, NONE]);
        assert_eq!(b.version, [0, 0, 4, 5]);
        b.send(HELLO, vec![map(&[("user_agent", s("test/1.0"))])]);
        b.success();
        let auth = match password {
            Some(p) => map(&[("scheme", s("basic")), ("principal", s("neo4j")), ("credentials", s(p))]),
            None => map(&[("scheme", s("none"))]),
        };
        b.send(LOGON, vec![auth]);
        b.success();
        b
    }

    fn send(&mut self, tag: u8, fields: Vec<PackValue>) {
        let mut buf = Vec::new();
        packstream::encode(&mut buf, &PackValue::Struct { tag, fields });
        let mut out = Vec::new();
        for chunk in buf.chunks(u16::MAX as usize) {
            out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&[0, 0]);
        self.s.write_all(&out).unwrap();
    }

    fn recv(&mut self) -> (u8, Vec<PackValue>) {
        let mut message = Vec::new();
        loop {
            let mut size = [0u8; 2];
            self.s.read_exact(&mut size).unwrap();
            let n = u16::from_be_bytes(size) as usize;
            if n == 0 {
                if message.is_empty() {
                    continue;
                }
                break;
            }
            let start = message.len();
            message.resize(start + n, 0);
            self.s.read_exact(&mut message[start..]).unwrap();
        }
        match packstream::decode(&message).unwrap() {
            PackValue::Struct { tag, fields } => (tag, fields),
            other => panic!("not a message: {other:?}"),
        }
    }

    /// Whether the server closed the connection.
    fn closed(&mut self) -> bool {
        let mut b = [0u8; 1];
        match self.s.read(&mut b) {
            Ok(0) => true,
            Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            Ok(_) => false,
        }
    }

    fn success(&mut self) -> BTreeMap<String, PackValue> {
        match self.recv() {
            (SUCCESS, mut f) => match f.pop() {
                Some(PackValue::Map(m)) => m,
                other => panic!("SUCCESS without a map: {other:?}"),
            },
            other => panic!("expected SUCCESS, got {other:?}"),
        }
    }

    /// Expects a FAILURE and returns its code.
    fn failure(&mut self) -> String {
        match self.recv() {
            (FAILURE, f) => match &f[0] {
                PackValue::Map(m) => match get(m, "code") {
                    PackValue::String(c) => c.clone(),
                    _ => panic!(),
                },
                _ => panic!(),
            },
            other => panic!("expected FAILURE, got {other:?}"),
        }
    }

    fn ignored(&mut self) {
        assert_eq!(self.recv().0, IGNORED);
    }

    fn run(&mut self, query: &str, params: PackValue, extra: PackValue) {
        self.send(RUN, vec![s(query), params, extra]);
    }

    fn pull(&mut self, n: i64) {
        self.send(PULL, vec![map(&[("n", PackValue::Int(n))])]);
    }

    /// Reads RECORDs up to the closing SUCCESS.
    fn records(&mut self) -> (Vec<Vec<PackValue>>, BTreeMap<String, PackValue>) {
        let mut rows = Vec::new();
        loop {
            match self.recv() {
                (RECORD, mut f) => match f.pop() {
                    Some(PackValue::List(values)) => rows.push(values),
                    other => panic!("{other:?}"),
                },
                (SUCCESS, mut f) => match f.pop() {
                    Some(PackValue::Map(m)) => return (rows, m),
                    other => panic!("{other:?}"),
                },
                other => panic!("expected RECORD or SUCCESS, got {other:?}"),
            }
        }
    }

    /// Runs a query with parameters and pulls every row.
    fn query_with(&mut self, query: &str, params: PackValue) -> (Vec<Vec<PackValue>>, BTreeMap<String, PackValue>) {
        self.run(query, params, empty());
        self.pull(-1);
        self.success();
        self.records()
    }

    fn query(&mut self, query: &str) -> Vec<Vec<PackValue>> {
        self.query_with(query, empty()).0
    }

    fn count(&mut self, label: &str) -> i64 {
        match self.query(&format!("MATCH (n:{label}) RETURN count(n)"))[0][0] {
            PackValue::Int(n) => n,
            ref other => panic!("{other:?}"),
        }
    }
}

#[test]
fn handshake_negotiates_a_version() {
    let server = start(ServerConfig::default());
    let addr = server.bolt_addr().unwrap();
    // A range: the newest version both know.
    assert_eq!(Bolt::handshake(addr, [[0, 8, 8, 5], V44, NONE, NONE]).version, [0, 0, 4, 5]);
    // Proposals are tried in order.
    assert_eq!(Bolt::handshake(addr, [[0, 0, 1, 5], V5, NONE, NONE]).version, [0, 0, 1, 5]);
    assert_eq!(Bolt::handshake(addr, [[0, 0, 1, 0xFF], V44, NONE, NONE]).version, [0, 0, 4, 4]);
    // Nothing in common: zeros, then the server hangs up.
    let mut b = Bolt::handshake(addr, [[0, 0, 0, 3], [0, 0, 3, 4], NONE, NONE]);
    assert_eq!(b.version, [0; 4]);
    assert!(b.closed());
    // Not Bolt at all.
    let mut raw = TcpStream::connect(addr).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    raw.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(raw.read(&mut buf).unwrap_or(0), 0);
}

#[test]
fn authentication() {
    let server = start(ServerConfig {
        token: Some("s3cret".into()),
        ..ServerConfig::default()
    });
    let addr = server.bolt_addr().unwrap();

    // 5.1+: HELLO, then LOGON. Queries before LOGON fail and close the connection.
    let mut b = Bolt::handshake(addr, [V5, NONE, NONE, NONE]);
    b.send(HELLO, vec![map(&[("user_agent", s("test"))])]);
    let hello = b.success();
    assert!(matches!(get(&hello, "server"), PackValue::String(s) if s.starts_with("Neo4j/5.4")));
    assert!(matches!(get(&hello, "connection_id"), PackValue::String(_)));
    b.run("RETURN 1", empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Security.Unauthorized");
    assert!(b.closed());

    let mut b = Bolt::handshake(addr, [V5, NONE, NONE, NONE]);
    b.send(HELLO, vec![map(&[("user_agent", s("test"))])]);
    b.success();
    b.send(LOGON, vec![map(&[("scheme", s("basic")), ("principal", s("x")), ("credentials", s("wrong"))])]);
    assert_eq!(b.failure(), "Neo.ClientError.Security.Unauthorized");
    assert!(b.closed());

    // Any user name with the token as the password, or the token as a bearer token.
    let mut b = Bolt::open_with(addr, Some("s3cret"));
    assert_eq!(b.query("RETURN 1"), vec![vec![PackValue::Int(1)]]);
    // LOGOFF needs a new LOGON.
    b.send(LOGOFF, vec![]);
    b.success();
    b.send(LOGON, vec![map(&[("scheme", s("bearer")), ("credentials", s("s3cret"))])]);
    b.success();
    assert_eq!(b.query("RETURN 2"), vec![vec![PackValue::Int(2)]]);

    // 4.4: the credentials come with HELLO.
    let mut b = Bolt::handshake(addr, [V44, NONE, NONE, NONE]);
    assert_eq!(b.version, [0, 0, 4, 4]);
    b.send(HELLO, vec![map(&[("user_agent", s("t")), ("scheme", s("basic")), ("principal", s("u")), ("credentials", s("s3cret"))])]);
    b.success();
    assert_eq!(b.query("RETURN 3"), vec![vec![PackValue::Int(3)]]);
    let mut b = Bolt::handshake(addr, [V44, NONE, NONE, NONE]);
    b.send(HELLO, vec![map(&[("user_agent", s("t")), ("scheme", s("none"))])]);
    assert_eq!(b.failure(), "Neo.ClientError.Security.Unauthorized");
    assert!(b.closed());

    // The first message must be HELLO.
    let mut b = Bolt::handshake(addr, [V5, NONE, NONE, NONE]);
    b.run("RETURN 1", empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    assert!(b.closed());
}

fn tag(v: &PackValue) -> (u8, &[PackValue]) {
    match v {
        PackValue::Struct { tag, fields } => (*tag, fields),
        other => panic!("not a structure: {other:?}"),
    }
}

#[test]
fn values_and_parameters() {
    let server = start(ServerConfig::default());
    let addr = server.bolt_addr().unwrap();
    let mut b = Bolt::open(addr);
    let (rows, summary) = b.query_with(
        "CREATE p = (a:Person {name: $name, tags: ['x']})-[r:KNOWS {since: 2020}]->(b:Person {name: 'B'})
         RETURN a, r, p, 1.5 AS f, null AS n, {k: [1, 'two']} AS m",
        map(&[("name", s("Ann"))]),
    );
    assert_eq!(get(&summary, "type"), &s("rw"));
    let stats = match get(&summary, "stats") {
        PackValue::Map(m) => m.clone(),
        _ => panic!(),
    };
    assert_eq!(get(&stats, "nodes-created"), &PackValue::Int(2));
    assert_eq!(get(&stats, "relationships-created"), &PackValue::Int(1));
    assert_eq!(get(&stats, "contains-updates"), &PackValue::Bool(true));
    assert!(matches!(get(&summary, "bookmark"), PackValue::String(_)));
    assert_eq!(get(&summary, "db"), &s("graphbuff"));

    let row = &rows[0];
    // Bolt 5 nodes: id, labels, properties, element id.
    let (t, node) = tag(&row[0]);
    assert_eq!(t, 0x4E);
    assert_eq!(node.len(), 4);
    assert_eq!(node[1], PackValue::List(vec![s("Person")]));
    assert_eq!(node[2], map(&[("name", s("Ann")), ("tags", PackValue::List(vec![s("x")]))]));
    let PackValue::Int(a_id) = node[0] else { panic!() };
    // Element ids are what `elementId()` returns.
    assert_eq!(node[3], s(&format!("n:{a_id}")));
    // Relationships: id, start, end, type, properties, and three element ids.
    let (t, rel) = tag(&row[1]);
    assert_eq!(t, 0x52);
    assert_eq!(rel.len(), 8);
    assert_eq!(rel[1], PackValue::Int(a_id));
    assert_eq!(rel[3], s("KNOWS"));
    assert_eq!(rel[4], map(&[("since", PackValue::Int(2020))]));
    // A path: distinct nodes, unbound relationships, indices.
    let (t, path) = tag(&row[2]);
    assert_eq!(t, 0x50);
    let PackValue::List(nodes) = &path[0] else { panic!() };
    let PackValue::List(rels) = &path[1] else { panic!() };
    assert_eq!((nodes.len(), rels.len()), (2, 1));
    assert_eq!(tag(&rels[0]).0, 0x72);
    assert_eq!(path[2], PackValue::List(vec![PackValue::Int(1), PackValue::Int(1)]));
    assert_eq!(row[3], PackValue::Float(1.5));
    assert_eq!(row[4], PackValue::Null);
    assert_eq!(row[5], map(&[("k", PackValue::List(vec![PackValue::Int(1), s("two")]))]));

    // A path walked against a relationship's direction has a negative index.
    let rows = b.query("MATCH p = (:Person {name: 'B'})<-[:KNOWS]-(:Person) RETURN p");
    let (_, path) = tag(&rows[0][0]);
    assert_eq!(path[2], PackValue::List(vec![PackValue::Int(-1), PackValue::Int(1)]));

    // Temporal and spatial values travel as structures, both ways.
    let rows = b.query(
        "RETURN date('2024-02-29') AS d, localtime('12:00:01') AS lt, time('10:00+02:00') AS t,
                localdatetime('1969-12-31T23:59:59.5') AS ldt, datetime('2024-01-01T12:00+01:00') AS dt,
                duration('P1M2DT3.5S') AS du, point({x: 1, y: 2}) AS p2,
                point({longitude: 10, latitude: 20, height: 30}) AS p3",
    );
    let row = &rows[0];
    assert_eq!(tag(&row[0]), (0x44, &[PackValue::Int(19_782)][..]));
    assert_eq!(tag(&row[1]), (0x74, &[PackValue::Int(43_201_000_000_000)][..]));
    assert_eq!(tag(&row[2]), (0x54, &[PackValue::Int(36_000_000_000_000), PackValue::Int(7200)][..]));
    assert_eq!(tag(&row[3]), (0x64, &[PackValue::Int(-1), PackValue::Int(500_000_000)][..]));
    // Bolt 5: the UTC instant (11:00Z) and the offset.
    assert_eq!(
        tag(&row[4]),
        (0x49, &[PackValue::Int(1_704_106_800), PackValue::Int(0), PackValue::Int(3600)][..])
    );
    assert_eq!(
        tag(&row[5]),
        (0x45, &[PackValue::Int(1), PackValue::Int(2), PackValue::Int(3), PackValue::Int(500_000_000)][..])
    );
    assert_eq!(
        tag(&row[6]),
        (0x58, &[PackValue::Int(7203), PackValue::Float(1.0), PackValue::Float(2.0)][..])
    );
    assert_eq!(tag(&row[7]).0, 0x59);
    // Sent back as parameters, they compare equal.
    let params = PackValue::Map(
        ["d", "lt", "t", "ldt", "dt", "du", "p2", "p3"]
            .iter()
            .zip(row)
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    );
    let (rows, _) = b.query_with(
        "RETURN $d = date('2024-02-29') AND $lt = localtime('12:00:01') AND $t = time('10:00+02:00')
            AND $ldt = localdatetime('1969-12-31T23:59:59.5') AND $dt = datetime('2024-01-01T12:00+01:00')
            AND $du = duration('P1M2DT3.5S') AND $p2 = point({x: 1, y: 2})
            AND $p3 = point({longitude: 10, latitude: 20, height: 30}), $dt.hour",
        params,
    );
    assert_eq!(rows[0], vec![PackValue::Bool(true), PackValue::Int(12)]);

    // A large value spans several chunks.
    let big = "x".repeat(200_000);
    let (rows, _) = b.query_with("RETURN size($s), $s", map(&[("s", s(&big))]));
    assert_eq!(rows[0], vec![PackValue::Int(200_000), s(&big)]);

    // Unsupported parameters fail; RESET recovers.
    b.run("RETURN $b", map(&[("b", PackValue::Bytes(vec![1]))]), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Statement.FeatureNotSupported");
    b.send(RESET, vec![]);
    b.success();
    let zoned = PackValue::Struct {
        tag: 0x69,
        fields: vec![PackValue::Int(0), PackValue::Int(0), s("Europe/Prague")],
    };
    b.run("RETURN $z", map(&[("z", zoned)]), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Statement.FeatureNotSupported");
    b.send(RESET, vec![]);
    b.success();

    // Bolt 4.4: no element ids, and local date-times unless the `utc` patch is asked for.
    for (patch, dt_tag, seconds) in [(false, 0x46, 1_704_110_400), (true, 0x49, 1_704_106_800)] {
        let mut b = Bolt::handshake(addr, [V44, NONE, NONE, NONE]);
        let mut hello = vec![("user_agent", s("t"))];
        if patch {
            hello.push(("patch_bolt", PackValue::List(vec![s("utc")])));
        }
        b.send(HELLO, vec![map(&hello)]);
        let meta = b.success();
        assert_eq!(meta.contains_key("patch_bolt"), patch);
        let rows = b.query("MATCH (a)-[r]->() RETURN a, r, datetime('2024-01-01T12:00+01:00')");
        assert_eq!(tag(&rows[0][0]).1.len(), 3);
        assert_eq!(tag(&rows[0][1]).1.len(), 5);
        assert_eq!(
            tag(&rows[0][2]),
            (dt_tag, &[PackValue::Int(seconds), PackValue::Int(0), PackValue::Int(3600)][..])
        );
    }
}

#[test]
fn pull_in_batches_and_discard() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    b.run("UNWIND range(1, 10) AS i RETURN i", empty(), empty());
    let meta = b.success();
    assert_eq!(get(&meta, "fields"), &PackValue::List(vec![s("i")]));
    assert!(!meta.contains_key("qid"));
    b.pull(3);
    let (rows, meta) = b.records();
    assert_eq!(rows, (1..=3).map(|i| vec![PackValue::Int(i)]).collect::<Vec<_>>());
    assert_eq!(meta, BTreeMap::from([("has_more".to_string(), PackValue::Bool(true))]));
    b.pull(5);
    let (rows, meta) = b.records();
    assert_eq!(rows.len(), 5);
    assert_eq!(get(&meta, "has_more"), &PackValue::Bool(true));
    // DISCARD drops the rest and ends the result.
    b.send(DISCARD, vec![map(&[("n", PackValue::Int(-1))])]);
    let (rows, meta) = b.records();
    assert!(rows.is_empty());
    assert_eq!(get(&meta, "type"), &s("r"));
    assert!(!meta.contains_key("stats"));

    // PULL with nothing open, or a bad `n`, fails.
    b.pull(-1);
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    b.send(RESET, vec![]);
    b.success();
    b.run("RETURN 1", empty(), empty());
    b.success();
    b.pull(0);
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    b.send(RESET, vec![]);
    b.success();
    // A second RUN while a result is open fails.
    b.run("RETURN 1", empty(), empty());
    b.success();
    b.run("RETURN 2", empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    b.send(RESET, vec![]);
    b.success();
    assert_eq!(b.query("RETURN 4"), vec![vec![PackValue::Int(4)]]);
    b.send(GOODBYE, vec![]);
    assert!(b.closed());
}

#[test]
fn pipelined_messages_and_failures() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    // Sent all at once, answered in order. After the failure, everything is IGNORED
    // until RESET.
    b.run("RETURN 1 AS x", empty(), empty());
    b.pull(-1);
    b.run("RETURN nope(", empty(), empty());
    b.pull(-1);
    b.run("RETURN 2", empty(), empty());
    b.success();
    assert_eq!(b.records().0, vec![vec![PackValue::Int(1)]]);
    assert_eq!(b.failure(), "Neo.ClientError.Statement.SyntaxError");
    b.ignored();
    b.ignored();
    b.send(RESET, vec![]);
    b.run("RETURN 3", empty(), empty());
    b.pull(-1);
    b.success();
    b.success();
    assert_eq!(b.records().0, vec![vec![PackValue::Int(3)]]);

    // Error codes.
    for (query, code) in [
        ("RETURN 1 +", "Neo.ClientError.Statement.SyntaxError"),
        ("RETURN x", "Neo.ClientError.Statement.SemanticError"),
        ("RETURN 1 + 'a' - 1", "Neo.ClientError.Statement.TypeError"),
    ] {
        b.run(query, empty(), empty());
        assert_eq!(b.failure(), code, "{query}");
        b.send(RESET, vec![]);
        b.success();
    }
    b.run("RETURN 1", empty(), map(&[("db", s("nope"))]));
    assert_eq!(b.failure(), "Neo.ClientError.Database.DatabaseNotFound");
    b.send(RESET, vec![]);
    b.success();

    // Databases are chosen per query with `db`; database commands work.
    b.query("CREATE DATABASE other");
    b.run("CREATE (:InOther)", empty(), map(&[("db", s("other"))]));
    b.success();
    b.pull(-1);
    let (_, meta) = b.records();
    assert_eq!(get(&meta, "db"), &s("other"));
    assert_eq!(b.count("InOther"), 0);
    b.run("MATCH (n:InOther) RETURN count(n)", empty(), map(&[("db", s("OTHER"))]));
    b.success();
    b.pull(-1);
    assert_eq!(b.records().0, vec![vec![PackValue::Int(1)]]);

    // TELEMETRY (5.4) is accepted.
    b.send(TELEMETRY, vec![PackValue::Int(0)]);
    b.success();
    // Unknown messages fail.
    b.send(0x55, vec![]);
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
}

#[test]
fn explain_and_profile_summaries() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    b.query("UNWIND range(1, 5) AS i CREATE (:N {i: i})");
    let (rows, meta) = b.query_with("EXPLAIN MATCH (n:N) WHERE n.i > 2 RETURN n.i", empty());
    assert!(rows.is_empty());
    let PackValue::Map(plan) = get(&meta, "plan") else { panic!() };
    assert!(matches!(get(plan, "operatorType"), PackValue::String(_)));
    assert!(matches!(get(plan, "children"), PackValue::List(c) if !c.is_empty()));
    let (rows, meta) = b.query_with("PROFILE MATCH (n:N) WHERE n.i > 2 RETURN n.i", empty());
    assert_eq!(rows.len(), 3);
    let PackValue::Map(profile) = get(&meta, "profile") else { panic!() };
    assert_eq!(get(profile, "rows"), &PackValue::Int(3));
}

#[test]
fn explicit_transactions() {
    let server = start(ServerConfig::default());
    let addr = server.bolt_addr().unwrap();
    let mut b = Bolt::open(addr);
    let mut other = Bolt::open(addr);

    // Commit: the statements see each other; others see nothing until the commit.
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.run("CREATE (:T {i: 1})", empty(), empty());
    assert_eq!(get(&b.success(), "qid"), &PackValue::Int(0));
    b.run("MATCH (t:T) RETURN count(t)", empty(), empty());
    assert_eq!(get(&b.success(), "qid"), &PackValue::Int(1));
    // Results stay open side by side; `qid` picks one, -1 is the latest.
    b.pull(-1);
    assert_eq!(b.records().0, vec![vec![PackValue::Int(1)]]);
    b.send(PULL, vec![map(&[("n", PackValue::Int(-1)), ("qid", PackValue::Int(0))])]);
    let (rows, meta) = b.records();
    assert!(rows.is_empty());
    assert!(!meta.contains_key("bookmark"));
    b.send(PULL, vec![map(&[("n", PackValue::Int(-1)), ("qid", PackValue::Int(0))])]);
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    b.send(RESET, vec![]); // rolls back
    b.success();
    assert_eq!(other.count("T"), 0);

    b.send(BEGIN, vec![map(&[("db", s("graphbuff")), ("tx_timeout", PackValue::Int(60_000))])]);
    b.success();
    b.run("CREATE (:T)", empty(), empty());
    b.success();
    assert_eq!(other.count("T"), 0);
    b.send(COMMIT, vec![]);
    // Unconsumed results are dropped at the commit.
    assert!(matches!(get(&b.success(), "bookmark"), PackValue::String(_)));
    assert_eq!(other.count("T"), 1);

    // Rollback.
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.run("CREATE (:T)", empty(), empty());
    b.success();
    b.send(ROLLBACK, vec![]);
    b.success();
    assert_eq!(b.count("T"), 1);

    // A failing statement ends the transaction: IGNORED until RESET, nothing kept.
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.run("CREATE (:T)", empty(), empty());
    b.success();
    b.run("RETURN 1 / 0", empty(), empty());
    b.pull(-1);
    assert_eq!(b.failure(), "Neo.ClientError.Statement.ArgumentError");
    b.send(COMMIT, vec![]);
    b.ignored();
    b.ignored();
    b.send(RESET, vec![]);
    b.success();
    assert_eq!(b.count("T"), 1);

    // USE of another database and database commands are refused.
    for q in ["USE other RETURN 1", "SHOW DATABASES"] {
        b.send(BEGIN, vec![empty()]);
        b.success();
        b.run(q, empty(), empty());
        b.failure();
        b.send(RESET, vec![]);
        b.success();
    }
    // COMMIT outside a transaction, and BEGIN inside one, fail.
    b.send(COMMIT, vec![]);
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    b.send(RESET, vec![]);
    b.success();
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.send(BEGIN, vec![empty()]);
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    b.send(RESET, vec![]);
    b.success();

    // Closing the connection rolls back.
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.run("CREATE (:T)", empty(), empty());
    b.success();
    drop(b);
    // Once rolled back, other writers go ahead.
    other.query("CREATE (:T)");
    assert_eq!(other.count("T"), 2);
}

#[test]
fn idle_transactions_time_out() {
    let server = start(ServerConfig {
        transaction_timeout: Duration::from_millis(300),
        ..ServerConfig::default()
    });
    let addr = server.bolt_addr().unwrap();
    let mut b = Bolt::open(addr);
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.run("CREATE (:T)", empty(), empty());
    b.success();
    // A writer elsewhere waits for the transaction, which is rolled back once idle.
    let started = Instant::now();
    let mut other = Bolt::open(addr);
    other.query("CREATE (:W)");
    assert!(started.elapsed() >= Duration::from_millis(200), "{:?}", started.elapsed());
    b.run("RETURN 1", empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Transaction.TransactionTimedOut");
    b.send(RESET, vec![]);
    b.success();
    assert_eq!(b.count("T"), 0);
    assert_eq!(b.count("W"), 1);
}

/// A billion rows: never finishes in a test.
const SLOW: &str = "MATCH (a:N), (b:N), (c:N) RETURN count(*)";

#[test]
fn timeouts_and_reset_interrupt_queries() {
    let server = start(ServerConfig {
        query_timeout: Some(Duration::from_millis(500)),
        ..ServerConfig::default()
    });
    let addr = server.bolt_addr().unwrap();
    let mut b = Bolt::open(addr);
    b.query("UNWIND range(1, 1000) AS i CREATE (:N)");

    // The server's limit, and a lower one asked for with `tx_timeout`.
    let started = Instant::now();
    b.run(SLOW, empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Transaction.TransactionTimedOut");
    assert!(started.elapsed() >= Duration::from_millis(500));
    b.send(RESET, vec![]);
    b.success();
    let started = Instant::now();
    b.run(SLOW, empty(), map(&[("tx_timeout", PackValue::Int(50))]));
    assert_eq!(b.failure(), "Neo.ClientError.Transaction.TransactionTimedOut");
    assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    b.send(RESET, vec![]);
    b.success();

    // RESET stops a running query: its RUN is IGNORED.
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    b.query("UNWIND range(1, 1000) AS i CREATE (:N)");
    b.send(BEGIN, vec![empty()]);
    b.success();
    b.run("CREATE (:T)", empty(), empty());
    b.success();
    b.run(SLOW, empty(), empty());
    b.pull(-1);
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    b.send(RESET, vec![]);
    b.ignored();
    b.ignored();
    b.success();
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(b.count("T"), 0);

    // Shutdown stops a running query and closes the connection promptly.
    b.run(SLOW, empty(), empty());
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    drop(server);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!(b.failure(), "Neo.ClientError.Transaction.Terminated");
    assert!(b.closed());
}

#[test]
fn route_names_this_server() {
    let server = start(ServerConfig::default());
    let addr = server.bolt_addr().unwrap();
    let mut b = Bolt::open(addr);
    b.send(
        ROUTE,
        vec![map(&[("address", s("db.example:7687"))]), PackValue::List(vec![]), empty()],
    );
    let meta = b.success();
    let PackValue::Map(rt) = get(&meta, "rt") else { panic!() };
    assert_eq!(get(rt, "db"), &s("graphbuff"));
    assert_eq!(get(rt, "ttl"), &PackValue::Int(300));
    let PackValue::List(servers) = get(rt, "servers") else { panic!() };
    assert_eq!(servers.len(), 3);
    for server in servers {
        let PackValue::Map(m) = server else { panic!() };
        assert_eq!(get(m, "addresses"), &PackValue::List(vec![s("db.example:7687")]));
    }
    // Without an address, the one the client connected to.
    b.send(ROUTE, vec![empty(), PackValue::List(vec![]), map(&[("db", PackValue::Null)])]);
    let meta = b.success();
    let PackValue::Map(rt) = get(&meta, "rt") else { panic!() };
    let PackValue::List(servers) = get(rt, "servers") else { panic!() };
    let PackValue::Map(m) = &servers[0] else { panic!() };
    assert_eq!(get(m, "addresses"), &PackValue::List(vec![s(&addr.to_string())]));
    b.send(ROUTE, vec![empty(), PackValue::List(vec![]), map(&[("db", s("missing"))])]);
    assert_eq!(b.failure(), "Neo.ClientError.Database.DatabaseNotFound");
}

#[test]
fn element_ids_match_the_element_id_function() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    let rows = b.query("CREATE (a)-[r:R]->(b) RETURN a, r, elementId(a), elementId(r), elementId(b)");
    let (_, node) = tag(&rows[0][0]);
    let (_, rel) = tag(&rows[0][1]);
    assert_eq!(node[3], rows[0][2]);
    assert_eq!(rel[5], rows[0][3]);
    assert_eq!(rel[6], rows[0][2]);
    assert_eq!(rel[7], rows[0][4]);
    let (found, _) = b.query_with("MATCH (n) WHERE elementId(n) = $id RETURN count(n)", map(&[("id", node[3].clone())]));
    assert_eq!(found, vec![vec![PackValue::Int(1)]]);
}

#[test]
fn query_types() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    let kind = |b: &mut Bolt, q: &str| b.query_with(q, empty()).1.get("type").cloned().unwrap();
    assert_eq!(kind(&mut b, "RETURN 1"), s("r"));
    assert_eq!(kind(&mut b, "CREATE ()"), s("rw"));
    assert_eq!(kind(&mut b, "CREATE INDEX i FOR (n:L) ON (n.p)"), s("s"));
    assert_eq!(kind(&mut b, "CREATE CONSTRAINT c FOR (n:M) REQUIRE n.p IS UNIQUE"), s("s"));
    assert_eq!(kind(&mut b, "DROP INDEX i"), s("s"));
}

#[test]
fn the_system_database_runs_database_commands() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    let system = || map(&[("db", s("system"))]);
    // Auto-commit.
    b.run("CREATE DATABASE other", empty(), system());
    b.pull(-1);
    b.success();
    let (_, meta) = b.records();
    assert_eq!(get(&meta, "type"), &s("s"));
    assert_eq!(get(&meta, "db"), &s("system"));
    b.run("SHOW DATABASES", empty(), system());
    b.pull(-1);
    b.success();
    let (rows, _) = b.records();
    assert_eq!(rows.len(), 2);
    // Anything else is refused, and the connection carries on.
    b.run("MATCH (n) RETURN n", empty(), system());
    assert_eq!(b.failure(), "Neo.ClientError.Statement.FeatureNotSupported");
    b.send(RESET, vec![]);
    b.success();
    // Explicit transactions, as drivers' managed transactions send them: the command
    // takes effect when it runs.
    b.send(BEGIN, vec![system()]);
    b.success();
    b.run("DROP DATABASE other", empty(), empty());
    b.success();
    b.pull(-1);
    b.records();
    b.run("MATCH (n) RETURN n", empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Statement.FeatureNotSupported");
    b.send(RESET, vec![]);
    b.success();
    b.send(BEGIN, vec![system()]);
    b.success();
    b.run("SHOW DATABASES", empty(), empty());
    b.success();
    b.pull(-1);
    let (rows, _) = b.records();
    assert_eq!(rows.len(), 1);
    b.send(COMMIT, vec![]);
    assert!(matches!(get(&b.success(), "bookmark"), PackValue::String(_)));
    // A real database named `system` wins.
    b.run("CREATE DATABASE system", empty(), empty());
    b.pull(-1);
    b.success();
    b.records();
    b.run("CREATE (:InSystem)", empty(), system());
    b.pull(-1);
    b.success();
    let (_, meta) = b.records();
    assert_eq!(get(&meta, "type"), &s("rw"));
}

#[test]
fn bolt_and_http_share_databases() {
    let server = start(ServerConfig::default());
    let mut b = Bolt::open(server.bolt_addr().unwrap());
    b.query("CREATE (:Shared)");
    let mut http = TcpStream::connect(server.local_addr()).unwrap();
    let body = r#"{"query":"MATCH (n:Shared) RETURN count(n)"}"#;
    write!(
        http,
        "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    http.read_to_string(&mut reply).unwrap();
    assert!(reply.ends_with(r#""rows":[[1]],"stats":{"nodesCreated":0,"nodesDeleted":0,"relationshipsCreated":0,"relationshipsDeleted":0,"propertiesSet":0,"labelsAdded":0,"labelsRemoved":0,"indexesAdded":0,"indexesRemoved":0,"constraintsAdded":0,"constraintsRemoved":0,"systemUpdates":0,"containsUpdates":false}}"#), "{reply}");
}

#[test]
fn oversized_and_malformed_messages_close_the_connection() {
    let server = start(ServerConfig {
        max_body_bytes: 1000,
        ..ServerConfig::default()
    });
    let addr = server.bolt_addr().unwrap();
    let mut b = Bolt::open(addr);
    b.run(&"x".repeat(2000), empty(), empty());
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    assert!(b.closed());

    let mut b = Bolt::open(addr);
    b.s.write_all(&[0, 2, 0xC7, 0x00, 0, 0]).unwrap(); // unknown marker
    assert_eq!(b.failure(), "Neo.ClientError.Request.Invalid");
    assert!(b.closed());
}
