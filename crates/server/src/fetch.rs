//! A small HTTP/1.1 client, over TLS when the build has it: `GET` for `LOAD CSV` from the
//! web (`storage.load_csv_urls`), following redirects only to allowed URLs, and `POST`
//! for the MCP bridge.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Redirects followed before giving up.
const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Checks `server.load_csv_urls`-style prefixes: comma-separated `http://` or `https://`
/// URLs with at least the `/` after the host, so that `https://example.com` cannot also
/// allow `https://example.com.evil.org`.
pub fn parse_prefixes(list: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for p in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let url = Url::parse(p).map_err(|e| format!("`{p}`: {e}"))?;
        if !p[url.scheme_len()..].contains('/') {
            return Err(format!("`{p}` needs a path, at least the `/` after the host: `{p}/`"));
        }
        out.push(p.to_string());
    }
    Ok(out)
}

/// What `LOAD CSV` may read from the web: URLs starting with one of `prefixes`.
pub fn csv_urls(prefixes: Vec<String>) -> graphbuff::CsvUrls {
    let allowed = prefixes.clone();
    graphbuff::CsvUrls { prefixes, open: Arc::new(move |url: &str| open(url, &allowed)) }
}

/// The body of `GET url`, following redirects to URLs that start with one of `allowed`.
pub fn open(url: &str, allowed: &[String]) -> io::Result<Box<dyn Read + Send>> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let parsed = Url::parse(&url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        match get(&parsed)? {
            Reply::Body(body) => return Ok(body),
            Reply::Redirect(location) => {
                let next = parsed.join(&location);
                if !allowed.iter().any(|p| next.starts_with(p.as_str())) {
                    return Err(io::Error::other(format!(
                        "redirected to {next}, which starts with none of the allowed URL prefixes"
                    )));
                }
                url = next;
            }
        }
    }
    Err(io::Error::other(format!("more than {MAX_REDIRECTS} redirects")))
}

/// A parsed `http://` or `https://` URL.
#[derive(Debug, PartialEq, Eq)]
struct Url {
    tls: bool,
    host: String,
    port: u16,
    /// From the `/` on, with the query; `/` when the URL has none.
    path: String,
}

impl Url {
    fn parse(s: &str) -> Result<Url, String> {
        let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
        let (tls, rest) = if lower.starts_with("https://") {
            (true, &s[8..])
        } else if lower.starts_with("http://") {
            (false, &s[7..])
        } else {
            return Err("only http:// and https:// URLs are supported".into());
        };
        let rest = rest.split('#').next().unwrap_or("");
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
            Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
            None => (rest, "/".to_string()),
        };
        if authority.contains('@') {
            return Err("credentials in the URL are not supported".into());
        }
        let default = if tls { 443 } else { 80 };
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, after) = v6.split_once(']').ok_or("unclosed `[`")?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| format!("bad port `{p}`"))?,
                None if after.is_empty() => default,
                None => return Err(format!("unexpected `{after}` after the host")),
            };
            (host.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().map_err(|_| format!("bad port `{p}`"))?),
                None => (authority.to_string(), default),
            }
        };
        if host.is_empty() {
            return Err("no host".into());
        }
        Ok(Url { tls, host, port, path })
    }

    fn scheme_len(&self) -> usize {
        if self.tls { 8 } else { 7 }
    }

    fn origin(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        let default = if self.tls { 443 } else { 80 };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == default { format!("{scheme}://{host}") } else { format!("{scheme}://{host}:{}", self.port) }
    }

    /// `location` (absolute, or relative to this URL) as an absolute URL.
    fn join(&self, location: &str) -> String {
        let lower = location.get(..8).unwrap_or(location).to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            location.to_string()
        } else if let Some(rest) = location.strip_prefix("//") {
            format!("{}://{rest}", if self.tls { "https" } else { "http" })
        } else if location.starts_with('/') {
            format!("{}{location}", self.origin())
        } else {
            let dir = self.path.split('?').next().unwrap_or("/");
            let dir = &dir[..=dir.rfind('/').unwrap_or(0)];
            format!("{}{dir}{location}", self.origin())
        }
    }
}

enum Reply {
    Body(Box<dyn Read + Send>),
    Redirect(String),
}

/// The TCP connection to `url`'s host.
fn open_tcp(url: &Url) -> io::Result<TcpStream> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other(format!("{} has no address", url.host)))?;
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
}

/// The `Host` header for `url`.
fn host_header(url: &Url) -> String {
    let host = if url.host.contains(':') { format!("[{}]", url.host) } else { url.host.clone() };
    if url.port == if url.tls { 443 } else { 80 } { host } else { format!("{host}:{}", url.port) }
}

/// Response headers: lower-cased names and their values.
type Headers = Vec<(String, String)>;

/// Reads a response's status, status line and headers.
fn head(r: &mut impl BufRead) -> io::Result<(u16, String, Headers)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP response: {:?}", line.trim_end())))?;
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            return Err(io::Error::other("the response ends in its headers"));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if headers.len() > 100 {
            return Err(io::Error::other("too many response headers"));
        }
        if let Some((n, v)) = h.split_once(':') {
            headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, line.trim_end().to_string(), headers))
}

/// The most bytes of an answer [`post`] reads.
const MAX_ANSWER: u64 = 64 << 20;

/// Checks that `url` is one [`post`] can send to.
pub(crate) fn check_url(url: &str) -> Result<(), String> {
    let u = Url::parse(url).map_err(|e| format!("`{url}`: {e}"))?;
    if u.tls && cfg!(not(any(feature = "tls", feature = "tls-rustcrypto"))) {
        return Err(format!("`{url}`: this build has no TLS for https:// URLs (build with the `tls` feature)"));
    }
    Ok(())
}

/// One `POST` of the JSON `body` to `url`, with `headers` besides the usual ones: the
/// status and the body of the answer. `opened` gets the TCP connection once it is open;
/// shutting it down aborts the request. Redirects are not followed.
pub(crate) fn post(
    url: &str,
    headers: &[(&str, String)],
    body: &[u8],
    opened: &mut dyn FnMut(&TcpStream),
) -> io::Result<(u16, Vec<u8>)> {
    let url = Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let tcp = open_tcp(&url)?;
    tcp.set_write_timeout(Some(READ_TIMEOUT))?;
    opened(&tcp);
    let mut stream = connect(&url, tcp)?;
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: graphbuff-server/{}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        url.path,
        host_header(&url),
        env!("CARGO_PKG_VERSION"),
        body.len()
    );
    for (n, v) in headers {
        if v.contains(['\r', '\n']) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a line break in header {n}")));
        }
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut r = BufReader::new(stream);
    let (status, _, headers) = head(&mut r)?;
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    let mut answer = Vec::new();
    if header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
        Chunked { r, left: 0, done: false }.take(MAX_ANSWER).read_to_end(&mut answer)?;
    } else if let Some(n) = header("content-length").and_then(|n| n.parse::<u64>().ok()) {
        if n > MAX_ANSWER {
            return Err(io::Error::other(format!("an answer of {n} bytes is too big")));
        }
        Exact { r: r.take(n), expected: n }.read_to_end(&mut answer)?;
    } else {
        r.take(MAX_ANSWER).read_to_end(&mut answer)?;
    }
    Ok((status, answer))
}

/// One `GET`.
fn get(url: &Url) -> io::Result<Reply> {
    let tcp = open_tcp(url)?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))?;
    tcp.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut stream = connect(url, tcp)?;
    let host = host_header(url);
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: graphbuff-server/{}\r\nAccept: text/csv, */*\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n\r\n",
        url.path,
        env!("CARGO_PKG_VERSION")
    )?;
    stream.flush()?;
    let mut r = BufReader::new(stream);
    let (status, line, headers) = head(&mut r)?;
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    match status {
        200 => {}
        301 | 302 | 303 | 307 | 308 => {
            let location = header("location").ok_or_else(|| io::Error::other(format!("{status} without a Location")))?;
            return Ok(Reply::Redirect(location.to_string()));
        }
        _ => return Err(io::Error::other(format!("the server answered {line}"))),
    }
    if header("content-encoding").is_some_and(|e| !e.eq_ignore_ascii_case("identity")) {
        return Err(io::Error::other("the server sent a Content-Encoding other than identity"));
    }
    if header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
        return Ok(Reply::Body(Box::new(Chunked { r, left: 0, done: false })));
    }
    Ok(Reply::Body(match header("content-length").and_then(|n| n.parse::<u64>().ok()) {
        Some(n) => Box::new(Exact { r: r.take(n), expected: n }),
        None => Box::new(r),
    }))
}

/// A body of a known length: ending early is an error, not a short file.
struct Exact<R: Read> {
    r: io::Take<R>,
    expected: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.r.read(buf)?;
        if n == 0 && !buf.is_empty() && self.r.limit() > 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the response ended after {} of {} bytes", self.expected - self.r.limit(), self.expected),
            ));
        }
        Ok(n)
    }
}

/// A `Transfer-Encoding: chunked` body.
struct Chunked<R: BufRead> {
    r: R,
    /// Bytes left in the current chunk.
    left: u64,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.r.read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("");
            self.left = u64::from_str_radix(size, 16).map_err(|_| io::Error::other("bad chunk size"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left as usize);
        let n = self.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the response ended inside a chunk"));
        }
        self.left -= n as u64;
        if self.left == 0 {
            let mut crlf = String::new();
            self.r.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

/// The connection: TCP, or TLS over it for `https://`.
fn connect(url: &Url, tcp: TcpStream) -> io::Result<Box<dyn ReadWrite>> {
    if !url.tls {
        return Ok(Box::new(tcp));
    }
    #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
    {
        let name = rustls::pki_types::ServerName::try_from(url.host.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let conn = rustls::ClientConnection::new(client_config()?, name).map_err(io::Error::other)?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            stream.conn.complete_io(&mut stream.sock)?;
        }
        Ok(Box::new(stream))
    }
    #[cfg(not(any(feature = "tls", feature = "tls-rustcrypto")))]
    {
        let _ = tcp;
        Err(io::Error::other("this build has no TLS for https:// URLs (build with the `tls` feature)"))
    }
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

/// CA bundles of common systems.
#[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
const SYSTEM_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
];

/// A CA file chosen on the command line, before any `https://` request.
static CA_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Trusts the certificates in `file` (PEM) for `https://`, instead of `SSL_CERT_FILE` or
/// the system's CA bundle. Only before the first `https://` request.
pub(crate) fn set_ca_file(file: PathBuf) -> Result<(), String> {
    std::fs::metadata(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    CA_FILE.set(file).map_err(|_| "the CA file is set already".to_string())
}

/// Trusts the certificates of [`set_ca_file`], else `SSL_CERT_FILE`, else the system's CA
/// bundle. Built once.
#[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
fn client_config() -> io::Result<Arc<rustls::ClientConfig>> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    static CONFIG: std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, String>> = std::sync::OnceLock::new();
    let built = CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        let files: Vec<std::path::PathBuf> = match (CA_FILE.get(), std::env::var_os("SSL_CERT_FILE")) {
            (Some(f), _) => vec![f.clone()],
            (None, Some(f)) => vec![f.into()],
            (None, None) => SYSTEM_BUNDLES.iter().map(Into::into).collect(),
        };
        for file in &files {
            if let Ok(it) = CertificateDer::pem_file_iter(file) {
                roots.add_parsable_certificates(it.filter_map(Result::ok));
            }
        }
        if roots.is_empty() {
            return Err("no CA certificates found to check https:// servers; set SSL_CERT_FILE to a PEM bundle \
                        (or pass --cacert to graphbuff-server mcp)"
                .into());
        }
        rustls::ClientConfig::builder_with_provider(Arc::new(crate::tls::provider()))
            .with_safe_default_protocol_versions()
            .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
            .map_err(|e| e.to_string())
    });
    built.clone().map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = Url::parse("https://Data.example.com/a/b.csv?x=1#frag").unwrap();
        assert_eq!(u, Url { tls: true, host: "Data.example.com".into(), port: 443, path: "/a/b.csv?x=1".into() });
        assert_eq!(Url::parse("http://h:8080").unwrap().path, "/");
        assert_eq!(Url::parse("http://h?q").unwrap().path, "/?q");
        assert_eq!(Url::parse("http://[::1]:9/x").unwrap().host, "::1");
        assert!(Url::parse("ftp://h/x").is_err());
        assert!(Url::parse("http://user:pw@h/x").is_err());
        assert_eq!(u.join("/c.csv"), "https://Data.example.com/c.csv");
        assert_eq!(u.join("c.csv"), "https://Data.example.com/a/c.csv");
        assert_eq!(u.join("http://other/x"), "http://other/x");
        assert_eq!(Url::parse("http://h:81/a").unwrap().join("/b"), "http://h:81/b");
    }

    #[test]
    fn prefixes() {
        assert_eq!(parse_prefixes(" https://a.example/, http://b.example/x/ ").unwrap().len(), 2);
        assert!(parse_prefixes("https://a.example").unwrap_err().contains("needs a path"));
        assert!(parse_prefixes("a.example/").is_err());
        assert!(parse_prefixes("").unwrap().is_empty());
    }

    #[test]
    fn chunked_bodies() {
        let mut body = String::new();
        Chunked { r: &b"4\r\nname\r\n5;x=y\r\n\nAnn\n\r\n0\r\n\r\n"[..], left: 0, done: false }
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "name\nAnn\n");
        let mut short = Vec::new();
        assert!(Exact { r: (&b"abc"[..]).take(5), expected: 5 }.read_to_end(&mut short).is_err());
    }
}
