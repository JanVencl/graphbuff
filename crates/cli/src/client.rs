//! A small HTTP/1.1 client for the JSON API of graphbuff-server: one keep-alive
//! connection, `Content-Length` or chunked replies, and the API's requests on top.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use graphbuff::json::{self, Json};

/// How long to wait for a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// An idle connection older than this is not reused: the server closes idle keep-alive
/// connections (after 30 seconds by default).
const MAX_IDLE: Duration = Duration::from_secs(20);

/// Where the server is: `http[s]://host[:port][/prefix]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// A path the API sits under (behind a reverse proxy), without a trailing `/`.
    pub prefix: String,
}

impl Url {
    /// Parses `http://host:port`, `https://host/prefix` or a bare `host[:port]` (HTTP on
    /// port 7475, the server's default). With a scheme and no port, the scheme's port.
    pub fn parse(s: &str) -> Result<Url, String> {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        let (tls, rest, scheme) = if lower.starts_with("http://") {
            (false, &s[7..], true)
        } else if lower.starts_with("https://") {
            (true, &s[8..], true)
        } else if s.contains("://") {
            return Err(format!("`{s}`: only http:// and https:// URLs are supported"));
        } else {
            (false, s, false)
        };
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if path.contains(['?', '#']) {
            return Err(format!("`{s}`: the URL may not have a query or fragment"));
        }
        if authority.contains('@') {
            return Err(format!("`{s}`: give the user with --user, not in the URL"));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, after) = v6.split_once(']').ok_or_else(|| format!("`{s}`: unclosed `[`"))?;
            match after {
                "" => (host, None),
                p => (host, Some(p.strip_prefix(':').ok_or_else(|| format!("`{s}`: bad port"))?)),
            }
        } else {
            match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(format!("`{s}`: no host"));
        }
        let port = match port {
            Some(p) => p.parse::<u16>().ok().filter(|&p| p > 0).ok_or_else(|| format!("`{s}`: bad port `{p}`"))?,
            None if !scheme => 7475,
            None if tls => 443,
            None => 80,
        };
        Ok(Url {
            tls,
            host: host.to_string(),
            port,
            prefix: path.trim_end_matches('/').to_string(),
        })
    }

    /// The `Host` header.
    fn host_header(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default = if self.tls { 443 } else { 80 };
        if self.port == default { host } else { format!("{host}:{}", self.port) }
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.host_header(), self.prefix)
    }
}

/// How the client logs in.
#[derive(Clone, Debug)]
pub enum Auth {
    None,
    /// The server's token: `Authorization: Bearer`.
    Token(String),
    /// A user account: `Authorization: Basic`.
    User { name: String, password: String },
}

/// Why a request failed.
#[derive(Debug)]
pub enum Error {
    /// No connection, or it broke.
    Connection(String),
    /// The server answered something that is not the API's JSON.
    Protocol(String),
    /// The server answered with an error object.
    Server {
        status: u16,
        code: String,
        message: String,
        /// For a syntax error: the byte of the query where it was found.
        offset: Option<usize>,
        /// The statement's transaction was rolled back.
        rolled_back: bool,
    },
}

impl Error {
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Error::Server { status: 401, .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connection(m) | Error::Protocol(m) => f.write_str(m),
            Error::Server { code, message, .. } => write!(f, "{code}: {message}"),
        }
    }
}

/// An HTTP reply.
struct Reply {
    status: u16,
    body: String,
}

/// The result of a statement.
#[derive(Debug, Default)]
pub struct Outcome {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Json>>,
    pub stats: BTreeMap<String, Json>,
    /// For `EXPLAIN` and `PROFILE`.
    pub plan: Option<Json>,
    /// The reply as the server sent it.
    pub raw: String,
}

/// An open explicit transaction.
#[derive(Clone, Debug)]
pub struct Tx {
    pub database: String,
    /// `/db/{name}/tx/{id}`.
    pub path: String,
}

pub struct Client {
    url: Url,
    auth: Auth,
    conn: Option<(BufReader<crate::tls::Stream>, Instant)>,
    tls: crate::tls::Connector,
}

impl Client {
    pub fn new(url: Url, auth: Auth, tls: crate::tls::Connector) -> Client {
        Client { url, auth, conn: None, tls }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    pub fn set_auth(&mut self, auth: Auth) {
        self.auth = auth;
        self.conn = None;
    }

    // ---- the API ------------------------------------------------------------------------

    /// `GET /db` as a result with one row per database.
    pub fn databases(&mut self) -> Result<Outcome, Error> {
        let raw = self.call("GET", "/db", None)?;
        let dbs = match json::parse(&raw) {
            Ok(Json::Object(mut m)) => match m.remove("databases") {
                Some(Json::Array(a)) => a,
                _ => return Err(Error::Protocol("the database list has no `databases`".into())),
            },
            _ => return Err(Error::Protocol("the database list is not a JSON object".into())),
        };
        let columns = ["name", "storage", "default", "nodes", "relationships"];
        let rows = dbs
            .into_iter()
            .map(|db| {
                let Json::Object(mut m) = db else { return vec![Json::Null; columns.len()] };
                columns.iter().map(|c| m.remove(*c).unwrap_or(Json::Null)).collect()
            })
            .collect();
        Ok(Outcome {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows,
            raw,
            ..Outcome::default()
        })
    }

    /// Runs `query`: in `tx` if given, else on its own against `database` (the server's
    /// default database if `None`).
    pub fn query(
        &mut self,
        database: Option<&str>,
        tx: Option<&Tx>,
        query: &str,
        params: &BTreeMap<String, Json>,
        timeout_ms: Option<u64>,
    ) -> Result<Outcome, Error> {
        let path = match (tx, database) {
            (Some(tx), _) => tx.path.clone(),
            (None, Some(db)) => format!("/db/{}/query", encode_segment(db)),
            (None, None) => "/query".to_string(),
        };
        let mut body = String::from(r#"{"query":"#);
        json::write_str(&mut body, query);
        if !params.is_empty() {
            body.push_str(r#","params":"#);
            write_json(&mut body, &Json::Object(params.clone()));
        }
        if let Some(ms) = timeout_ms {
            let _ = write!(body, r#","timeoutMs":{ms}"#);
        }
        body.push('}');
        let raw = self.call("POST", &path, Some(&body))?;
        decode_outcome(raw)
    }

    /// Begins a transaction on `database` (the default database if `None`).
    pub fn begin(&mut self, database: Option<&str>) -> Result<Tx, Error> {
        let path = match database {
            Some(db) => format!("/db/{}/tx", encode_segment(db)),
            None => "/tx".into(),
        };
        let raw = self.call("POST", &path, Some("{}"))?;
        let field = |m: &BTreeMap<String, Json>, k: &str| match m.get(k) {
            Some(Json::String(s)) => Some(s.clone()),
            _ => None,
        };
        if let Ok(Json::Object(mut m)) = json::parse(&raw)
            && let Some(Json::Object(t)) = m.remove("transaction")
            && let (Some(database), Some(path)) = (field(&t, "database"), field(&t, "path"))
        {
            return Ok(Tx { database, path });
        }
        Err(Error::Protocol("the server did not describe the new transaction".into()))
    }

    pub fn commit(&mut self, tx: &Tx) -> Result<(), Error> {
        self.call("POST", &format!("{}/commit", tx.path), Some("{}")).map(drop)
    }

    pub fn rollback(&mut self, tx: &Tx) -> Result<(), Error> {
        self.call("DELETE", &tx.path, None).map(drop)
    }

    /// A request whose reply must be `2xx`; an error reply becomes [`Error::Server`].
    fn call(&mut self, method: &str, path: &str, body: Option<&str>) -> Result<String, Error> {
        let reply = self.request(method, path, body)?;
        if (200..300).contains(&reply.status) {
            return Ok(reply.body);
        }
        Err(decode_error(reply))
    }

    // ---- HTTP ---------------------------------------------------------------------------

    fn request(&mut self, method: &str, path: &str, body: Option<&str>) -> Result<Reply, Error> {
        if self.conn.as_ref().is_some_and(|(_, used)| used.elapsed() > MAX_IDLE) {
            self.conn = None;
        }
        let reused = self.conn.is_some();
        match self.attempt(method, path, body) {
            // The server closed the idle connection before it saw the request: try once
            // more on a new one.
            Err(Attempt::Stale(_)) if reused => {
                self.conn = None;
                self.attempt(method, path, body).map_err(Attempt::into_error)
            }
            r => r.map_err(Attempt::into_error),
        }
    }

    fn attempt(&mut self, method: &str, path: &str, body: Option<&str>) -> Result<Reply, Attempt> {
        if self.conn.is_none() {
            let stream = self.connect().map_err(Attempt::Failed)?;
            self.conn = Some((BufReader::with_capacity(64 << 10, stream), Instant::now()));
        }
        let mut head = format!(
            "{method} {}{path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: graphbuff-cli/{}\r\nAccept: application/json\r\n",
            self.url.prefix,
            self.url.host_header(),
            env!("CARGO_PKG_VERSION"),
        );
        match &self.auth {
            Auth::None => {}
            Auth::Token(t) => {
                let _ = write!(head, "Authorization: Bearer {t}\r\n");
            }
            Auth::User { name, password } => {
                let creds = base64(format!("{name}:{password}").as_bytes());
                let _ = write!(head, "Authorization: Basic {creds}\r\n");
            }
        }
        let body = body.unwrap_or("");
        if !body.is_empty() || method == "POST" {
            let _ = write!(head, "Content-Type: application/json\r\nContent-Length: {}\r\n", body.len());
        }
        head.push_str("\r\n");

        let (conn, _) = self.conn.as_mut().expect("connected");
        let sent = (|| {
            let w = conn.get_mut();
            w.write_all(head.as_bytes())?;
            w.write_all(body.as_bytes())?;
            w.flush()
        })();
        if let Err(e) = sent {
            self.conn = None;
            return Err(Attempt::Stale(e));
        }
        match read_reply(conn) {
            Ok((reply, keep)) => {
                if keep {
                    self.conn.as_mut().expect("connected").1 = Instant::now();
                } else {
                    self.conn = None;
                }
                Ok(reply)
            }
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    fn connect(&self) -> io::Result<crate::tls::Stream> {
        let mut addrs: Vec<_> = (self.url.host.as_str(), self.url.port).to_socket_addrs()?.collect();
        // `localhost` is often `::1` first, but the server listens on 127.0.0.1 by default,
        // and a refused connection can take seconds (on Windows): IPv4 first for loopback.
        if addrs.iter().all(|a| a.ip().is_loopback()) {
            addrs.sort_by_key(|a| a.is_ipv6());
        }
        let mut last = io::Error::new(io::ErrorKind::NotFound, "the host name has no address");
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(tcp) => {
                    tcp.set_nodelay(true)?;
                    return self.tls.wrap(&self.url, tcp);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

/// How an attempt at a request failed.
enum Attempt {
    /// Nothing came back on a connection: it may have been closed while idle.
    Stale(io::Error),
    Failed(io::Error),
    Bad(String),
}

impl Attempt {
    fn into_error(self) -> Error {
        match self {
            Attempt::Stale(e) | Attempt::Failed(e) => Error::Connection(e.to_string()),
            Attempt::Bad(m) => Error::Protocol(m),
        }
    }
}

/// Reads a reply, skipping `1xx` ones. Also says whether the connection stays open.
fn read_reply(r: &mut impl BufRead) -> Result<(Reply, bool), Attempt> {
    let mut first = true;
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) if first => {
                return Err(Attempt::Stale(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the server closed the connection",
                )));
            }
            Ok(0) => return Err(Attempt::Failed(io::ErrorKind::UnexpectedEof.into())),
            Err(e) if first => return Err(Attempt::Stale(e)),
            Err(e) => return Err(Attempt::Failed(e)),
            Ok(_) => {}
        }
        first = false;
        let mut parts = line.split_whitespace();
        let status = match (parts.next(), parts.next().and_then(|s| s.parse::<u16>().ok())) {
            (Some(v), Some(s)) if v.starts_with("HTTP/1.") => s,
            _ => return Err(Attempt::Bad(format!("not an HTTP reply: {:?}", line.trim_end()))),
        };
        let keep_default = line.starts_with("HTTP/1.1");

        let mut length = None;
        let mut chunked = false;
        let mut close = !keep_default;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).map_err(Attempt::Failed)? == 0 {
                return Err(Attempt::Failed(io::ErrorKind::UnexpectedEof.into()));
            }
            let h = h.trim_end_matches(['\r', '\n']);
            if h.is_empty() {
                break;
            }
            let Some((name, value)) = h.split_once(':') else { continue };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    length = Some(value.parse::<usize>().map_err(|_| Attempt::Bad("bad Content-Length".into()))?)
                }
                "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
                "connection" => {
                    let v = value.to_ascii_lowercase();
                    close = v.contains("close") || (!keep_default && !v.contains("keep-alive"));
                }
                _ => {}
            }
        }
        if (100..200).contains(&status) {
            continue;
        }
        let mut body = Vec::new();
        if chunked {
            read_chunked(r, &mut body)?;
        } else if let Some(n) = length {
            body.resize(n, 0);
            r.read_exact(&mut body).map_err(Attempt::Failed)?;
        } else if status != 204 && status != 304 {
            r.read_to_end(&mut body).map_err(Attempt::Failed)?;
            close = true;
        }
        let body = String::from_utf8(body).map_err(|_| Attempt::Bad("the reply is not UTF-8".into()))?;
        return Ok((Reply { status, body }, !close));
    }
}

fn read_chunked(r: &mut impl BufRead, body: &mut Vec<u8>) -> Result<(), Attempt> {
    loop {
        let mut line = String::new();
        r.read_line(&mut line).map_err(Attempt::Failed)?;
        let size = line.trim().split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size.trim(), 16).map_err(|_| Attempt::Bad("bad chunk size".into()))?;
        if size == 0 {
            // Trailers, up to the empty line.
            loop {
                let mut t = String::new();
                if r.read_line(&mut t).map_err(Attempt::Failed)? == 0 || t.trim().is_empty() {
                    return Ok(());
                }
            }
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..]).map_err(Attempt::Failed)?;
        let mut crlf = [0; 2];
        r.read_exact(&mut crlf).map_err(Attempt::Failed)?;
    }
}

fn decode_error(reply: Reply) -> Error {
    if let Ok(Json::Object(mut m)) = json::parse(&reply.body) {
        let rolled_back = matches!(m.get("rolledBack"), Some(Json::Bool(true)));
        if let Some(Json::Object(mut e)) = m.remove("error") {
            let text = |v: Option<Json>| match v {
                Some(Json::String(s)) => s,
                _ => String::new(),
            };
            let offset = match e.get("offset") {
                Some(Json::Int(o)) if *o >= 0 => Some(*o as usize),
                _ => None,
            };
            return Error::Server {
                status: reply.status,
                code: text(e.remove("code")),
                message: text(e.remove("message")),
                offset,
                rolled_back,
            };
        }
    }
    let excerpt: String = reply.body.chars().take(200).collect();
    Error::Protocol(format!("HTTP {}: {}", reply.status, excerpt.trim()))
}

fn decode_outcome(raw: String) -> Result<Outcome, Error> {
    let Ok(Json::Object(mut m)) = json::parse(&raw) else {
        return Err(Error::Protocol("the result is not a JSON object".into()));
    };
    // A result streamed by the server that failed after its first rows went out.
    if m.contains_key("error") {
        return Err(decode_error(Reply { status: 200, body: raw }));
    }
    let columns = match m.remove("columns") {
        Some(Json::Array(c)) => c
            .into_iter()
            .map(|c| match c {
                Json::String(s) => s,
                other => compact(&other),
            })
            .collect(),
        _ => Vec::new(),
    };
    let rows = match m.remove("rows") {
        Some(Json::Array(rows)) => rows
            .into_iter()
            .map(|r| match r {
                Json::Array(v) => v,
                other => vec![other],
            })
            .collect(),
        _ => Vec::new(),
    };
    let stats = match m.remove("stats") {
        Some(Json::Object(s)) => s,
        _ => BTreeMap::new(),
    };
    Ok(Outcome {
        columns,
        rows,
        stats,
        plan: m.remove("plan"),
        raw,
    })
}

/// Percent-encodes one path segment.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Writes `j` as JSON.
pub fn write_json(out: &mut String, j: &Json) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Float(f) => json::write_float(out, *f),
        Json::String(s) => json::write_str(out, s),
        Json::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, x);
            }
            out.push(']');
        }
        Json::Object(m) => {
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json::write_str(out, k);
                out.push(':');
                write_json(out, v);
            }
            out.push('}');
        }
    }
}

/// `j` as compact JSON text.
pub fn compact(j: &Json) -> String {
    let mut s = String::new();
    write_json(&mut s, j);
    s
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = Url::parse("localhost").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.prefix.as_str()), (false, "localhost", 7475, ""));
        let u = Url::parse("https://example.com/graph/").unwrap();
        assert_eq!((u.tls, u.host.as_str(), u.port, u.prefix.as_str()), (true, "example.com", 443, "/graph"));
        assert_eq!(u.to_string(), "https://example.com/graph");
        let u = Url::parse("http://[::1]:9000").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 9000));
        assert_eq!(u.host_header(), "[::1]:9000");
        assert!(Url::parse("ftp://x").is_err());
        assert!(Url::parse("http://u:p@x").is_err());
        assert!(Url::parse("http://x:0").is_err());
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn an_error_after_streamed_rows() {
        let raw = r#"{"columns":["x"],"rows":[[1]],"error":{"code":"RuntimeError","message":"division by zero"}}"#;
        let e = decode_outcome(raw.into()).unwrap_err();
        assert_eq!(e.to_string(), "RuntimeError: division by zero");
    }

    #[test]
    fn chunked_reply() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let (reply, keep) = read_reply(&mut &raw[..]).ok().unwrap();
        assert_eq!((reply.status, reply.body.as_str(), keep), (200, "{\"a\":1}", true));
    }
}
