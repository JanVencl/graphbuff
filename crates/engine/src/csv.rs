//! `LOAD CSV`: finding the file and reading its records, without dependencies.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};

use graphbuff_core::{Error, Result};

/// The file `url` names inside the import directory `dir`. `file:///a/b.csv`,
/// `file:a/b.csv` and `a/b.csv` all mean `dir/a/b.csv`; a path that would leave `dir`
/// (`..`, a drive or root of its own after the URL's slashes) is rejected.
pub(crate) fn resolve(dir: Option<&Path>, url: &str) -> Result<PathBuf> {
    let Some(dir) = dir else {
        return Err(Error::Runtime(
            "LOAD CSV is disabled: no import directory is set \
             (Database::set_import_dir, or --import-dir for the CLI and server)"
                .into(),
        ));
    };
    let rest = if let Some(r) = url.strip_prefix("file:") {
        percent_decode(r.trim_start_matches('/'))?
    } else if url.contains("://") {
        return Err(Error::Unsupported(format!("LOAD CSV only reads local files, not `{url}`")));
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
    Ok(path)
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
    input: BufReader<File>,
    terminator: u8,
    path: PathBuf,
    /// Lines read so far (for error messages).
    line: u64,
    record_line: u64,
    started: bool,
}

impl CsvReader {
    pub fn open(path: PathBuf, terminator: u8) -> Result<CsvReader> {
        let file = File::open(&path)
            .map_err(|e| Error::Runtime(format!("cannot open {}: {e}", path.display())))?;
        Ok(CsvReader {
            input: BufReader::new(file),
            terminator,
            path,
            line: 0,
            record_line: 0,
            started: false,
        })
    }

    fn byte(&mut self) -> Result<Option<u8>> {
        let buf = self
            .input
            .fill_buf()
            .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", self.path.display())))?;
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
            .map_err(|e| Error::Runtime(format!("cannot read {}: {e}", self.path.display())))?;
        Ok(buf.first().copied())
    }

    fn text(&self, bytes: Vec<u8>) -> Result<String> {
        String::from_utf8(bytes).map_err(|_| {
            Error::Runtime(format!("{} line {}: not valid UTF-8", self.path.display(), self.line + 1))
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
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
                                    self.path.display()
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
        assert_eq!(resolve(Some(dir), "file:///a/b.csv").unwrap(), dir.join("a").join("b.csv"));
        assert_eq!(resolve(Some(dir), "b%20c.csv").unwrap(), dir.join("b%20c.csv"));
        assert_eq!(resolve(Some(dir), "file:///b%20c.csv").unwrap(), dir.join("b c.csv"));
        assert!(resolve(Some(dir), "file:///../x.csv").is_err());
        assert!(resolve(Some(dir), "a/../../x.csv").is_err());
        assert!(resolve(Some(dir), "/etc/passwd").is_err());
        assert!(resolve(Some(dir), "https://example.com/x.csv").is_err());
        assert!(resolve(None, "x.csv").is_err());
    }
}
