//! The server's log: `graphbuff.log` (start-up, settings, connections, failed logins,
//! errors) and `query.log` (queries slower than [`LogConfig::slow_query`]), one line per
//! event, each starting with a UTC timestamp. Files rotate by size: `graphbuff.log`
//! becomes `graphbuff.log.1`, and so on, keeping [`LogConfig::keep`] old files.
//!
//! Without a directory both go to standard error, which suits containers.
//!
//! ```text
//! 2026-11-15T18:00:00.123Z INFO  graphbuff-server 0.3.0 listening on http://127.0.0.1:7475
//! 2026-11-15T18:00:05.004Z WARN  failed login (bolt) from 10.0.0.7:51234
//! 2026-11-15T18:00:09.871Z SLOW  1532 ms bolt neo4j alice MATCH (a)-[*..6]->(b) RETURN count(b)
//! ```
//!
//! Query parameters are never logged.

use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How much the log says. Each level includes the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    /// Also every connection.
    Debug,
}

impl Level {
    /// `error`, `warn`, `info` or `debug`.
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
}

/// Settings for [`Log::open`].
#[derive(Clone, Debug)]
pub struct LogConfig {
    /// Where `graphbuff.log` and `query.log` go (created if missing). `None`: standard
    /// error.
    pub dir: Option<PathBuf>,
    pub level: Level,
    /// Log queries that take at least this long to `query.log`. `None`: no query log.
    pub slow_query: Option<Duration>,
    /// Rotate a file when it would grow past this size.
    pub max_bytes: u64,
    /// Rotated files kept per log.
    pub keep: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            dir: None,
            level: Level::Info,
            slow_query: None,
            max_bytes: 10 << 20,
            keep: 5,
        }
    }
}

/// The server's log; see the [module documentation](self).
pub struct Log {
    level: Level,
    slow_query: Option<Duration>,
    dir: Option<PathBuf>,
    events: Mutex<Sink>,
    queries: Mutex<Sink>,
}

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Log")
            .field("level", &self.level)
            .field("slow_query", &self.slow_query)
            .field("dir", &self.dir)
            .finish()
    }
}

/// Longest query text written to `query.log`.
const MAX_QUERY: usize = 4000;

impl Log {
    pub fn open(config: LogConfig) -> io::Result<Log> {
        let sink = |name: &str| -> io::Result<Sink> {
            match &config.dir {
                Some(dir) => Ok(Sink::File(Rotating::open(dir.join(name), config.max_bytes, config.keep)?)),
                None => Ok(Sink::Stderr),
            }
        };
        if let Some(dir) = &config.dir {
            fs::create_dir_all(dir)?;
        }
        Ok(Log {
            level: config.level,
            slow_query: config.slow_query,
            events: Mutex::new(sink("graphbuff.log")?),
            queries: Mutex::new(sink("query.log")?),
            dir: config.dir,
        })
    }

    /// The directory the files are in; `None` when logging to standard error.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The slow-query threshold, if the query log is on.
    pub fn slow_query(&self) -> Option<Duration> {
        self.slow_query
    }

    pub fn enabled(&self, level: Level) -> bool {
        level <= self.level
    }

    pub fn log(&self, level: Level, message: impl Display) {
        if self.enabled(level) {
            let line = format!("{} {} {}\n", timestamp(SystemTime::now()), level.tag(), one_line(&message.to_string()));
            self.events.lock().unwrap_or_else(|e| e.into_inner()).write(&line);
        }
    }

    pub fn error(&self, message: impl Display) {
        self.log(Level::Error, message);
    }

    pub fn warn(&self, message: impl Display) {
        self.log(Level::Warn, message);
    }

    pub fn info(&self, message: impl Display) {
        self.log(Level::Info, message);
    }

    pub fn debug(&self, message: impl Display) {
        self.log(Level::Debug, message);
    }

    /// Records a query that took `elapsed`, if that is slow enough.
    pub(crate) fn query(&self, elapsed: Duration, protocol: &str, database: &str, user: &str, text: &str) {
        let Some(limit) = self.slow_query else { return };
        if elapsed < limit {
            return;
        }
        let mut text = one_line(text.trim());
        if text.len() > MAX_QUERY {
            let mut end = MAX_QUERY;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        let line = format!(
            "{} SLOW  {} ms {protocol} {database} {user} {text}\n",
            timestamp(SystemTime::now()),
            elapsed.as_millis()
        );
        self.queries.lock().unwrap_or_else(|e| e.into_inner()).write(&line);
    }
}

/// Newlines and tabs become spaces, so that one event stays one line.
fn one_line(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

enum Sink {
    Stderr,
    File(Rotating),
}

impl Sink {
    /// Writes a line; a log that cannot be written must not stop the server.
    fn write(&mut self, line: &str) {
        match self {
            Sink::Stderr => {
                let _ = io::stderr().lock().write_all(line.as_bytes());
            }
            Sink::File(f) => {
                if f.write(line.as_bytes()).is_err() {
                    let _ = io::stderr().lock().write_all(line.as_bytes());
                }
            }
        }
    }
}

/// A file that moves to `<name>.1` (and older ones to `.2`, …) when it is full.
struct Rotating {
    path: PathBuf,
    /// `None` only while rotating (Windows cannot rename an open file).
    file: Option<File>,
    size: u64,
    max: u64,
    keep: usize,
}

impl Rotating {
    fn open(path: PathBuf, max: u64, keep: usize) -> io::Result<Rotating> {
        let file = Self::append(&path)?;
        let size = file.metadata()?.len();
        Ok(Rotating {
            path,
            file: Some(file),
            size,
            max: max.max(1),
            keep,
        })
    }

    fn append(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size + bytes.len() as u64 > self.max {
            self.rotate()?;
        }
        let file = match &mut self.file {
            Some(f) => f,
            None => self.file.insert(Self::append(&self.path)?),
        };
        file.write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let numbered = |n: usize| {
            let mut name = self.path.clone().into_os_string();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        };
        self.file = None;
        if self.keep == 0 {
            File::create(&self.path)?;
        } else {
            let _ = fs::remove_file(numbered(self.keep));
            for n in (1..self.keep).rev() {
                let _ = fs::rename(numbered(n), numbered(n + 1));
            }
            fs::rename(&self.path, numbered(1))?;
        }
        self.size = 0;
        self.file = Some(Self::append(&self.path)?);
        Ok(())
    }
}

/// `2026-11-15T18:00:00.123Z`.
fn timestamp(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// The date `days` after 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_794_765_431_801);
        assert_eq!(timestamp(t), "2026-11-15T17:57:11.801Z");
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn levels_filter_and_parse() {
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse("loud"), None);
        assert!(Level::Error < Level::Debug);
    }

    #[test]
    fn files_rotate_and_keep_a_limit() {
        let dir = std::env::temp_dir().join(format!("graphbuff-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = Log::open(LogConfig {
            dir: Some(dir.clone()),
            level: Level::Info,
            slow_query: Some(Duration::from_millis(10)),
            max_bytes: 200,
            keep: 2,
        })
        .unwrap();
        for i in 0..20 {
            log.info(format!("event number {i} with some padding to fill the file"));
        }
        log.debug("not written");
        log.query(Duration::from_millis(5), "http", "neo4j", "-", "fast");
        log.query(Duration::from_millis(50), "bolt", "neo4j", "alice", "MATCH (n)\nRETURN n");
        drop(log);
        let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
        assert!(dir.join("graphbuff.log.1").exists() && dir.join("graphbuff.log.2").exists());
        assert!(!dir.join("graphbuff.log.3").exists());
        assert!(read("graphbuff.log").contains("event number 19"));
        assert!(!read("graphbuff.log").contains("not written"));
        let q = read("query.log");
        assert!(q.contains("SLOW  50 ms bolt neo4j alice MATCH (n) RETURN n"), "{q}");
        assert!(!q.contains("fast"));
        let _ = fs::remove_dir_all(&dir);
    }
}
