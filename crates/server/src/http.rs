//! Minimal HTTP/1.1: reading requests (with keep-alive, `Content-Length` or chunked
//! bodies, and `Expect: 100-continue`) and writing responses.

use std::io::{self, BufRead, BufReader, Read, Write};
use crate::net::Stream;
use std::time::{Duration, Instant};

/// Longest request line or header line.
const MAX_LINE: usize = 8 * 1024;
const MAX_HEADERS: usize = 100;

pub struct Limits {
    pub max_body: usize,
    /// How long an idle keep-alive connection is kept open.
    pub idle: Duration,
    /// How long one request may take to arrive once it has started.
    pub request: Duration,
}

pub struct Request {
    pub method: String,
    /// The path without the query string.
    pub path: String,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(&'static str, String)>,
    pub content_type: &'static str,
}

impl Response {
    pub fn json(status: u16, body: String) -> Response {
        Response::text(status, body, "application/json; charset=utf-8")
    }

    /// A response with a body of another type than JSON.
    pub fn text(status: u16, body: String, content_type: &'static str) -> Response {
        Response {
            status,
            body,
            headers: Vec::new(),
            content_type,
        }
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Response {
        self.headers.push((name, value.into()));
        self
    }
}

/// A request that could not be read.
pub enum ReadError {
    /// Malformed or too large: answer with this status and message, then close.
    Bad(u16, String),
    /// The connection failed or timed out: just close it.
    Io,
}

fn bad(status: u16, msg: impl Into<String>) -> ReadError {
    ReadError::Bad(status, msg.into())
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Reads from a stream with a short read timeout, retrying until `deadline`.
struct Timed<'a> {
    r: &'a mut BufReader<Stream>,
    deadline: Instant,
}

impl Timed<'_> {
    fn fill(&mut self) -> Result<&[u8], ReadError> {
        loop {
            match self.r.fill_buf() {
                Ok(_) => break,
                Err(e) if is_timeout(&e) && Instant::now() < self.deadline => {}
                Err(_) => return Err(ReadError::Io),
            }
        }
        Ok(self.r.buffer())
    }

    /// One line without its line ending.
    fn line(&mut self) -> Result<String, ReadError> {
        let mut line = Vec::new();
        loop {
            let buf = self.fill()?;
            if buf.is_empty() {
                return Err(ReadError::Io); // closed mid-request
            }
            let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (&buf[..=i], true),
                None => (buf, false),
            };
            line.extend_from_slice(chunk);
            let n = chunk.len();
            self.r.consume(n);
            if line.len() > MAX_LINE {
                return Err(bad(431, "request line or header is too long"));
            }
            if done {
                break;
            }
        }
        while let Some(b'\n' | b'\r') = line.last() {
            line.pop();
        }
        String::from_utf8(line).map_err(|_| bad(400, "request head is not UTF-8"))
    }

    fn exact(&mut self, out: &mut Vec<u8>, mut n: usize) -> Result<(), ReadError> {
        while n > 0 {
            let buf = self.fill()?;
            if buf.is_empty() {
                return Err(ReadError::Io);
            }
            let take = n.min(buf.len());
            out.extend_from_slice(&buf[..take]);
            self.r.consume(take);
            n -= take;
        }
        Ok(())
    }
}

/// Reads the next request. Returns `None` when the client closed the connection, it
/// stayed idle too long, or `stop()` became true while it was idle.
pub fn read_request(
    reader: &mut BufReader<Stream>,
    writer: &mut Stream,
    limits: &Limits,
    stop: &dyn Fn() -> bool,
) -> Result<Option<Request>, ReadError> {
    // Wait for the first byte of a request.
    let idle_since = Instant::now();
    loop {
        match reader.fill_buf() {
            Ok([]) => return Ok(None),
            Ok(_) => break,
            Err(e) if is_timeout(&e) => {
                if stop() || idle_since.elapsed() > limits.idle {
                    return Ok(None);
                }
            }
            Err(_) => return Ok(None),
        }
    }

    let mut t = Timed {
        r: reader,
        deadline: Instant::now() + limits.request,
    };
    let mut request_line = t.line()?;
    if request_line.is_empty() {
        request_line = t.line()?; // tolerate a stray CRLF between requests
    }
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad(400, "malformed request line"));
    };
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return Err(bad(505, "only HTTP/1.1 and HTTP/1.0 are supported")),
    };

    let mut headers = Vec::new();
    loop {
        let line = t.line()?;
        if line.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Err(bad(431, "too many headers"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(bad(400, "malformed header"));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };

    let connection = header("connection").unwrap_or("").to_ascii_lowercase();
    let keep_alive = if http10 {
        connection.contains("keep-alive")
    } else {
        !connection.contains("close")
    };
    let chunked = header("transfer-encoding")
        .is_some_and(|te| te.to_ascii_lowercase().contains("chunked"));
    let length = match header("content-length") {
        Some(_) if chunked => return Err(bad(400, "both Content-Length and chunked encoding")),
        Some(v) => Some(
            v.parse::<usize>()
                .map_err(|_| bad(400, "invalid Content-Length"))?,
        ),
        None => None,
    };
    if length.is_some_and(|n| n > limits.max_body) {
        return Err(bad(413, "request body is too large"));
    }
    if header("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
        && (chunked || length.unwrap_or(0) > 0)
    {
        writer
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|_| ReadError::Io)?;
    }

    let mut body = Vec::new();
    if chunked {
        loop {
            let line = t.line()?;
            let size = line.split(';').next().unwrap_or("").trim();
            let size =
                usize::from_str_radix(size, 16).map_err(|_| bad(400, "invalid chunk size"))?;
            if size == 0 {
                // Trailers, up to the empty line.
                while !t.line()?.is_empty() {}
                break;
            }
            if body.len() + size > limits.max_body {
                return Err(bad(413, "request body is too large"));
            }
            t.exact(&mut body, size)?;
            if !t.line()?.is_empty() {
                return Err(bad(400, "chunk is longer than its size"));
            }
        }
    } else if let Some(n) = length {
        body.reserve(n);
        t.exact(&mut body, n)?;
    }

    let path = target.split('?').next().unwrap_or("").to_string();
    Ok(Some(Request {
        method: method.to_string(),
        path,
        headers,
        body,
        keep_alive,
    }))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

pub fn write_response(w: &mut Stream, resp: &Response, keep_alive: bool) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    );
    for (name, value) in &resp.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    // One write for small responses keeps Nagle's algorithm out of the way.
    let mut out = head.into_bytes();
    out.extend_from_slice(resp.body.as_bytes());
    w.write_all(&out)?;
    w.flush()
}

/// Starts a `200` response whose body follows in chunks ([`write_chunk`], [`end_chunks`]),
/// with `headers` added.
pub fn write_chunked_head(
    w: &mut Stream,
    content_type: &str,
    keep_alive: bool,
    headers: &[(&'static str, String)],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: {}\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())
}

/// One chunk of a chunked body (nothing for no bytes: an empty chunk ends the body).
pub fn write_chunk(w: &mut Stream, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let mut out = format!("{:X}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    w.write_all(&out)
}

/// Ends a chunked body.
pub fn end_chunks(w: &mut Stream) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")?;
    w.flush()
}

/// Where a handler writes its response: it returns a [`Response`], or writes one itself
/// (a streamed result) and marks it written.
pub struct Out<'w> {
    pub w: &'w mut Stream,
    pub keep_alive: bool,
    /// Headers every response to the request carries (CORS).
    pub headers: Vec<(&'static str, String)>,
    /// The handler wrote the response; a failed write closes the connection.
    pub written: Option<io::Result<()>>,
}

/// Reads from `r` until it would block, so that request bytes a client sent before we
/// answered with an error are not left unread (which can turn the close into a reset).
pub fn drain(r: &mut BufReader<Stream>) {
    let mut sink = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        match r.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Cross-origin requests from browser apps (`server.cors_origins`): which page origins may
/// call the API.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cors {
    /// Allowed origins (`scheme://host[:port]`, lower-cased); `"*"` allows every origin.
    origins: Vec<String>,
}

/// Methods and request headers a preflight allows.
const CORS_METHODS: &str = "GET, POST, DELETE";
const CORS_HEADERS: &str = "Authorization, Content-Type, Accept";

impl Cors {
    /// From a comma-separated list of origins, such as
    /// `"https://app.example.com, http://localhost:5173"`, or `"*"`. An empty list turns
    /// CORS off.
    pub fn parse(list: &str) -> Result<Cors, String> {
        let mut origins = Vec::new();
        for o in list.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            if o != "*" {
                let rest = o
                    .strip_prefix("https://")
                    .or_else(|| o.strip_prefix("http://"))
                    .ok_or_else(|| format!("`{o}` is not an origin like https://app.example.com (or *)"))?;
                if rest.is_empty() || rest.contains('/') {
                    return Err(format!("`{o}` is not an origin like https://app.example.com (scheme and host, no path)"));
                }
            }
            origins.push(o.to_ascii_lowercase());
        }
        Ok(Cors { origins })
    }

    /// Whether any origin is allowed.
    pub fn is_on(&self) -> bool {
        !self.origins.is_empty()
    }

    /// The allowed `Origin` of `req`, if it has one.
    fn allowed<'r>(&self, req: &'r Request) -> Option<&'r str> {
        let origin = req.header("origin")?;
        let lower = origin.to_ascii_lowercase();
        self.origins.iter().any(|o| o == "*" || *o == lower).then_some(origin)
    }

    /// The headers a response to `req` carries: none unless it comes from an allowed origin.
    pub fn headers(&self, req: &Request) -> Vec<(&'static str, String)> {
        let Some(origin) = self.allowed(req) else {
            return Vec::new();
        };
        vec![
            ("Access-Control-Allow-Origin", origin.to_string()),
            ("Access-Control-Expose-Headers", "Retry-After, WWW-Authenticate".into()),
            ("Vary", "Origin".into()),
        ]
    }

    /// The answer to a preflight (`OPTIONS` with `Access-Control-Request-Method`) from an
    /// allowed origin. Preflights carry no credentials, so this comes before logging in.
    pub fn preflight(&self, req: &Request) -> Option<Response> {
        if req.method != "OPTIONS" || req.header("access-control-request-method").is_none() {
            return None;
        }
        self.allowed(req)?;
        let mut r = Response::text(204, String::new(), "text/plain");
        r.headers = self.headers(req);
        r.headers.push(("Access-Control-Allow-Methods", CORS_METHODS.into()));
        r.headers.push(("Access-Control-Allow-Headers", CORS_HEADERS.into()));
        r.headers.push(("Access-Control-Max-Age", "600".into()));
        Some(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.into(),
            path: "/db".into(),
            headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
            body: Vec::new(),
            keep_alive: true,
        }
    }

    #[test]
    fn cors_origins() {
        assert!(!Cors::parse("").unwrap().is_on());
        assert!(Cors::parse("app.example.com").is_err());
        assert!(Cors::parse("https://app.example.com/x").is_err());
        let c = Cors::parse("https://App.example.com, http://localhost:5173").unwrap();
        let from = |o: &str| c.headers(&req("GET", &[("origin", o)]));
        assert_eq!(from("https://app.example.com")[0].1, "https://app.example.com");
        assert_eq!(from("http://localhost:5173").len(), 3);
        assert!(from("https://evil.example").is_empty());
        assert!(c.headers(&req("GET", &[])).is_empty());
        let pre = |o: &str| c.preflight(&req("OPTIONS", &[("origin", o), ("access-control-request-method", "POST")]));
        assert_eq!(pre("http://localhost:5173").unwrap().status, 204);
        assert!(pre("https://evil.example").is_none());
        assert!(c.preflight(&req("OPTIONS", &[("origin", "http://localhost:5173")])).is_none());
        let any = Cors::parse("*").unwrap();
        assert_eq!(any.headers(&req("GET", &[("origin", "https://x.dev")]))[0].1, "https://x.dev");
    }
}
