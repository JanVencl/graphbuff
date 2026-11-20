//! `LOAD CSV`: finding the file and reading its records, without dependencies. A file
//! (or response) that starts like gzip is decompressed as it is read.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use graphbuff_core::{Error, Result};

/// Opens an `http://` or `https://` URL for `LOAD CSV`: the body of the response.
pub type UrlOpener = dyn Fn(&str) -> std::io::Result<Box<dyn Read + Send>> + Send + Sync;

/// Lets `LOAD CSV` read `http://` and `https://` URLs that start with one of `prefixes`
/// (such as `https://data.example.com/exports/`), through `open`, which also decides about
/// redirects. Set with [`crate::Database::set_csv_urls`] or
/// [`crate::DatabaseManager::set_csv_urls`]; `graphbuff-server` sets it from
/// `storage.load_csv_urls`.
#[derive(Clone)]
pub struct CsvUrls {
    pub prefixes: Vec<String>,
    pub open: Arc<UrlOpener>,
}

impl std::fmt::Debug for CsvUrls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsvUrls").field("prefixes", &self.prefixes).finish_non_exhaustive()
    }
}

/// What a `LOAD CSV` URL names.
pub(crate) enum Source {
    File(PathBuf),
    Url(String),
}

/// Whether `url` is an `http://` or `https://` URL.
fn is_web(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// What `url` names: a file inside the import directory `dir`, or (with `urls`) a web
/// URL with an allowed prefix. `file:///a/b.csv`, `file:a/b.csv` and `a/b.csv` all mean
/// `dir/a/b.csv`; a path that would leave `dir` (`..`, a drive or root of its own after the
/// URL's slashes) is rejected.
pub(crate) fn resolve(dir: Option<&Path>, urls: Option<&CsvUrls>, url: &str) -> Result<Source> {
    if is_web(url) {
        let Some(urls) = urls else {
            return Err(Error::Unsupported(format!(
                "LOAD CSV reads `{url}` only when web URLs are allowed \
                 (Database::set_csv_urls, or load_csv_urls in [storage] of the server's config file)"
            )));
        };
        if !urls.prefixes.iter().any(|p| url.starts_with(p.as_str())) {
            return Err(Error::Runtime(format!(
                "LOAD CSV may not read `{url}`: it starts with none of the allowed URL prefixes"
            )));
        }
        return Ok(Source::Url(url.to_string()));
    }
    let Some(dir) = dir else {
        return Err(Error::Runtime(
            "LOAD CSV is disabled: no import directory is set \
             (Database::set_import_dir, or import_dir in [storage] of the server's config file)"
                .into(),
        ));
    };
    let rest = if let Some(r) = url.strip_prefix("file:") {
        percent_decode(r.trim_start_matches('/'))?
    } else if url.contains("://") {
        return Err(Error::Unsupported(format!("LOAD CSV reads local files and http(s) URLs, not `{url}`")));
    } else {
        url.to_string()
    };
    let mut path = dir.to_path_buf();
    for c in Path::new(&rest).components() {
        match c {
            Component::Normal(p) => path.push(p),
            Component::CurDir => {}
            _ => {
                return Err(Error::Runtime(format!(
                    "LOAD CSV path `{url}` must stay inside the import directory"
                )));
            }
        }
    }
    Ok(Source::File(path))
}

/// Opens what `resolve` found.
pub(crate) fn open(source: Source, urls: Option<&CsvUrls>, terminator: u8) -> Result<CsvReader> {
    match source {
        Source::File(path) => CsvReader::open(path, terminator),
        Source::Url(url) => {
            let open = &urls.expect("resolve checked").open;
            let body = open(&url).map_err(|e| Error::Runtime(format!("LOAD CSV cannot read {url}: {e}")))?;
            CsvReader::from_reader(url, body, terminator)
        }
    }
}

fn percent_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(b) = hex else {
                return Err(Error::Runtime(format!("invalid percent escape in `{s}`")));
            };
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::Runtime(format!("`{s}` is not valid UTF-8")))
}

/// One field: its text, and whether it was quoted (an unquoted empty field is null).
pub(crate) type Field = (String, bool);

/// Reads CSV records (RFC 4180: quoted fields may hold the terminator, line breaks and
/// doubled quotes) with a one-byte field terminator. Records end at `\n`, `\r\n` or `\r`;
/// empty lines are skipped.
pub(crate) struct CsvReader {
    input: BufReader<Box<dyn Read + Send>>,
    terminator: u8,
    /// The file or URL, for messages.
    name: String,
    /// Lines read so far (for error messages).
    line: u64,
    record_line: u64,
    started: bool,
}

impl CsvReader {
    pub fn open(path: PathBuf, terminator: u8) -> Result<CsvReader> {
        let file = File::open(&path)
            .map_err(|e| Error::Runtime(format!("cannot open {}: {e}", path.display())))?;
        Self::from_reader(path.display().to_string(), Box::new(file), terminator)
    }

    /// Records from `input`, decompressed if it starts like gzip.
    pub fn from_reader(name: String, input: Box<dyn Read + Send>, terminator: u8) -> Result<CsvReader> {
        let mut raw = BufReader::with_capacity(64 * 1024, input);
        let gzip = crate::gzip::is_gzip(raw.fill_buf().map_err(|e| Error::Runtime(format!("cannot read {name}: {e}")))?);
        let input: Box<dyn Read + Send> = if gzip { Box::new(crate::gzip::GzDecoder::new(raw)) } else { Box::new(raw) };
        Ok(CsvReader {
            input: BufReader::with_capacity(64 * 1024, input),
            terminator,
            name,
            line: 0,
            record_line: 0,
            started: false,
        })
    }

    fn byte(&mut self) -> Result<Option<u8>> {
        let buf = self
            .input
            .fill_buf()
            .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", self.name)))?;
        let Some(&b) = buf.first() else {
            return Ok(None);
        };
        self.input.consume(1);
        Ok(Some(b))
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        let buf = self
            .input
            .fill_buf()
            .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", self.name)))?;
        Ok(buf.first().copied())
    }

    fn text(&self, bytes: Vec<u8>) -> Result<String> {
        String::from_utf8(bytes).map_err(|_| {
            Error::Runtime(format!("{} line {}: not valid UTF-8", self.name, self.line + 1))
        })
    }

    /// The file or URL read.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The line the record just returned starts on (1-based).
    pub fn record_line(&self) -> u64 {
        self.record_line
    }

    /// The next record, or `None` at the end of the file.
    pub fn next_record(&mut self) -> Result<Option<Vec<Field>>> {
        if !self.started {
            self.started = true;
            let bom = [0xEF, 0xBB, 0xBF];
            if self.input.fill_buf().map(|b| b.starts_with(&bom)).unwrap_or(false) {
                self.input.consume(3);
            }
        }
        loop {
            self.record_line = self.line + 1;
            let mut fields: Vec<Field> = Vec::new();
            let mut cur: Vec<u8> = Vec::new();
            let mut quoted = false;
            let mut any = false;
            loop {
                let Some(b) = self.byte()? else {
                    if !any {
                        return Ok(None);
                    }
                    fields.push((self.text(cur)?, quoted));
                    return Ok(Some(fields));
                };
                any = true;
                if b == self.terminator {
                    fields.push((self.text(std::mem::take(&mut cur))?, quoted));
                    quoted = false;
                } else if b == b'\n' || b == b'\r' {
                    if b == b'\r' && self.peek()? == Some(b'\n') {
                        self.byte()?;
                    }
                    self.line += 1;
                    break;
                } else if b == b'"' && cur.is_empty() && !quoted {
                    quoted = true;
                    let start = self.line + 1;
                    loop {
                        match self.byte()? {
                            None => {
                                return Err(Error::Runtime(format!(
                                    "{} line {start}: unterminated quoted field",
                                    self.name
                                )));
                            }
                            Some(b'"') if self.peek()? == Some(b'"') => {
                                self.byte()?;
                                cur.push(b'"');
                            }
                            Some(b'"') => break,
                            Some(c) => {
                                if c == b'\n' {
                                    self.line += 1;
                                }
                                cur.push(c);
                            }
                        }
                    }
                } else {
                    cur.push(b);
                }
            }
            // An empty line is no record.
            if fields.is_empty() && cur.is_empty() && !quoted {
                continue;
            }
            fields.push((self.text(cur)?, quoted));
            return Ok(Some(fields));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, terminator: u8) -> Vec<Vec<Field>> {
        let dir = std::env::temp_dir().join(format!("graphbuff-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("t{}.csv", text.len()));
        std::fs::write(&path, text).unwrap();
        let mut r = CsvReader::open(path.clone(), terminator).unwrap();
        let mut out = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            out.push(rec);
        }
        std::fs::remove_file(path).unwrap();
        out
    }

    fn f(s: &str, q: bool) -> Field {
        (s.to_string(), q)
    }

    #[test]
    fn records_and_fields() {
        assert_eq!(read("a,b\r\n1,2\n", b','), vec![vec![f("a", false), f("b", false)], vec![f("1", false), f("2", false)]]);
        assert_eq!(read("x,,\"\"\n\n", b','), vec![vec![f("x", false), f("", false), f("", true)]]);
        assert_eq!(read("\"a,\"\"b\"\"\nc\";d", b';'), vec![vec![f("a,\"b\"\nc", true), f("d", false)]]);
        assert_eq!(read("\u{feff}h\tv\n", b'\t'), vec![vec![f("h", false), f("v", false)]]);
        assert_eq!(read("", b','), Vec::<Vec<Field>>::new());
    }

    #[test]
    fn paths_stay_inside_the_import_directory() {
        let dir = Path::new("imports");
        let file = |url: &str| match resolve(Some(dir), None, url) {
            Ok(Source::File(p)) => Ok(p),
            Ok(Source::Url(u)) => panic!("{u}"),
            Err(e) => Err(e),
        };
        assert_eq!(file("file:///a/b.csv").unwrap(), dir.join("a").join("b.csv"));
        assert_eq!(file("b%20c.csv").unwrap(), dir.join("b%20c.csv"));
        assert_eq!(file("file:///b%20c.csv").unwrap(), dir.join("b c.csv"));
        assert!(file("file:///../x.csv").is_err());
        assert!(file("a/../../x.csv").is_err());
        assert!(file("/etc/passwd").is_err());
        assert!(matches!(file("https://example.com/x.csv"), Err(Error::Unsupported(_))));
        assert!(file("ftp://example.com/x.csv").is_err());
        assert!(resolve(None, None, "x.csv").is_err());
    }

    #[test]
    fn urls_need_an_allowed_prefix() {
        let urls = CsvUrls {
            prefixes: vec!["https://data.example.com/".into()],
            open: Arc::new(|_| Ok(Box::new(std::io::empty()) as Box<dyn Read + Send>)),
        };
        assert!(matches!(
            resolve(None, Some(&urls), "https://data.example.com/a.csv"),
            Ok(Source::Url(u)) if u == "https://data.example.com/a.csv"
        ));
        assert!(resolve(None, Some(&urls), "https://data.example.com.evil.org/a.csv").is_err());
        assert!(resolve(None, Some(&urls), "http://data.example.com/a.csv").is_err());
        // Files still need the import directory.
        assert!(resolve(None, Some(&urls), "a.csv").is_err());
    }

    #[test]
    fn gzipped_input_is_decompressed() {
        let gz = include_bytes!("../tests/data/people.csv.gz");
        let mut r = CsvReader::from_reader("people.csv.gz".into(), Box::new(&gz[..]), b',').unwrap();
        let mut n = 0;
        let mut last = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            n += 1;
            last = rec;
        }
        assert_eq!(n, 5_001);
        assert_eq!(last, vec![f("person 4999", false), f("4999", false), f("city 19", false)]);
    }
}
