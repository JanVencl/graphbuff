//! Running input line by line: Cypher statements ended by `;`, and `:` commands.
//! The interactive prompt, scripts and `--query` text all go through [`Shell::line`].

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::Instant;

use graphbuff::json::Json;

use crate::client::{Client, Error, Tx};
use crate::format::{self, Format};
use crate::split;

/// How deep `:source` may nest.
const MAX_SOURCE_DEPTH: usize = 16;

pub const HELP: &str = "\
Statements end with `;` and may span lines. Commands:
  :use [NAME]           run statements against database NAME (none: the default database)
  :dbs                  list the databases
  :begin                begin a transaction; :commit or :rollback ends it
  :param NAME EXPR      set parameter $NAME to the value of the Cypher expression EXPR
  :params [clear]       show (or remove) the parameters
  :format [FORMAT]      show or set the output: table, plain, csv or json
  :timeout [MS|off]     show or set the time limit of each statement
  :source FILE          run the statements and commands in FILE
  :help                 this help
  :quit                 leave (an open transaction is rolled back)";

/// Whether to go on reading.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Flow {
    Go,
    Quit,
}

pub struct Shell {
    pub client: Client,
    pub database: Option<String>,
    pub tx: Option<Tx>,
    pub params: BTreeMap<String, Json>,
    pub format: Format,
    pub timeout_ms: Option<u64>,
    /// Stop at the first failure (scripts).
    pub stop_on_error: bool,
    /// Statements and commands that failed.
    pub failures: usize,
    buffer: String,
    depth: usize,
}

impl Shell {
    pub fn new(client: Client, format: Format, interactive: bool) -> Shell {
        Shell {
            client,
            database: None,
            tx: None,
            params: BTreeMap::new(),
            format,
            timeout_ms: None,
            stop_on_error: !interactive,
            failures: 0,
            buffer: String::new(),
            depth: 0,
        }
    }

    /// Whether a statement has begun but not ended.
    pub fn pending(&self) -> bool {
        !self.buffer.is_empty() && !split::is_blank(&self.buffer)
    }

    pub fn prompt(&self) -> String {
        let db = match (&self.tx, &self.database) {
            (Some(tx), _) => tx.database.clone(),
            (None, Some(db)) => db.clone(),
            (None, None) => "graphbuff".into(),
        };
        let tx = if self.tx.is_some() { "(tx)" } else { "" };
        let main = format!("{db}{tx}> ");
        if self.pending() {
            format!("{}> ", " ".repeat(main.chars().count().saturating_sub(2)))
        } else {
            main
        }
    }

    /// Takes one line of input.
    pub fn line(&mut self, line: &str) -> Flow {
        let trimmed = line.trim();
        if !self.pending() && trimmed.starts_with(':') {
            self.buffer.clear();
            return self.command(trimmed);
        }
        self.buffer.push_str(line);
        self.buffer.push('\n');
        let (done, rest) = split::split(&self.buffer);
        if done.is_empty() {
            return Flow::Go;
        }
        self.buffer = if rest.is_empty() { rest } else { rest + "\n" };
        for statement in done {
            if self.statement(&statement).is_err() && self.stop_on_error {
                return Flow::Quit;
            }
        }
        Flow::Go
    }

    /// Ends the input: runs a last statement without `;`, and rolls back a transaction
    /// left open.
    pub fn finish(&mut self) {
        let rest = std::mem::take(&mut self.buffer);
        let (mut statements, rest) = split::split(&rest);
        if !rest.is_empty() {
            statements.push(rest);
        }
        for s in statements {
            if self.statement(&s).is_err() && self.stop_on_error {
                break;
            }
        }
        if let Some(tx) = self.tx.take() {
            match self.client.rollback(&tx) {
                Ok(()) => note("the open transaction was rolled back"),
                Err(e) => note(&format!("rolling back the open transaction: {e}")),
            }
        }
    }

    /// Runs one statement and writes its result.
    fn statement(&mut self, text: &str) -> Result<(), ()> {
        let started = Instant::now();
        let result = self.client.query(
            self.database.as_deref(),
            self.tx.as_ref(),
            text,
            &self.params,
            self.timeout_ms,
        );
        let elapsed = started.elapsed();
        match result {
            Ok(r) => {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                if let Err(e) = format::write(&mut out, self.format, &r, elapsed).and_then(|()| out.flush()) {
                    output_failed(e);
                }
                if self.tx.is_none()
                    && let Some(db) = bare_use(text)
                {
                    self.database = Some(db);
                }
                Ok(())
            }
            Err(e) => {
                self.failed(&e, Some(text));
                Err(())
            }
        }
    }

    fn failed(&mut self, e: &Error, statement: Option<&str>) {
        self.failures += 1;
        eprintln!("{e}");
        if let (Error::Server { offset: Some(offset), .. }, Some(text)) = (e, statement) {
            eprint!("{}", pointer(text, *offset));
        }
        if let Error::Server { rolled_back, code, .. } = e
            && self.tx.is_some()
            && (*rolled_back || code == "TransactionNotFound")
        {
            self.tx = None;
            note("the transaction was rolled back");
        }
    }

    fn command(&mut self, text: &str) -> Flow {
        let (name, arg) = match text.find(char::is_whitespace) {
            Some(i) => (&text[..i], text[i..].trim()),
            None => (text, ""),
        };
        let arg = arg.trim_end_matches(';').trim();
        let ok = match name.to_ascii_lowercase().as_str() {
            ":quit" | ":exit" | ":q" => return Flow::Quit,
            ":help" | ":h" | ":?" => {
                println!("{HELP}");
                Ok(())
            }
            ":use" => self.use_database(arg),
            ":dbs" | ":databases" => self.list_databases(),
            ":begin" => self.begin(),
            ":commit" => self.end(true),
            ":rollback" => self.end(false),
            ":param" => self.set_param(arg),
            ":params" => self.show_params(arg),
            ":format" => self.set_format(arg),
            ":timeout" => self.set_timeout(arg),
            ":source" => self.source(arg),
            _ => Err(format!("unknown command `{name}`; :help lists the commands")),
        };
        match ok {
            Ok(()) => Flow::Go,
            Err(m) => {
                if !m.is_empty() {
                    eprintln!("{m}");
                    self.failures += 1;
                }
                if self.stop_on_error { Flow::Quit } else { Flow::Go }
            }
        }
    }

    fn use_database(&mut self, arg: &str) -> Result<(), String> {
        if self.tx.is_some() {
            return Err("end the transaction before switching databases".into());
        }
        let name = unquote_name(arg);
        self.database = (!name.is_empty()).then_some(name);
        Ok(())
    }

    fn list_databases(&mut self) -> Result<(), String> {
        let started = Instant::now();
        match self.client.databases() {
            Ok(r) => {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                if let Err(e) = format::write(&mut out, self.format, &r, started.elapsed()) {
                    output_failed(e);
                }
                Ok(())
            }
            Err(e) => {
                self.failed(&e, None);
                Err(String::new())
            }
        }
    }

    fn begin(&mut self) -> Result<(), String> {
        if self.tx.is_some() {
            return Err("a transaction is already open; :commit or :rollback it first".into());
        }
        match self.client.begin(self.database.as_deref()) {
            Ok(tx) => {
                note(&format!("transaction begun on database {}", tx.database));
                self.tx = Some(tx);
                Ok(())
            }
            Err(e) => {
                self.failed(&e, None);
                Err(String::new())
            }
        }
    }

    fn end(&mut self, commit: bool) -> Result<(), String> {
        let Some(tx) = self.tx.take() else {
            return Err("no transaction is open; :begin starts one".into());
        };
        let result = if commit { self.client.commit(&tx) } else { self.client.rollback(&tx) };
        match result {
            Ok(()) => {
                note(if commit { "committed" } else { "rolled back" });
                Ok(())
            }
            Err(e) => {
                // A failed commit has rolled back; a failed rollback leaves nothing to do.
                self.failed(&e, None);
                Err(String::new())
            }
        }
    }

    /// `:param name expr`, also `name => expr`, `name: expr` and `name = expr`; the
    /// server evaluates the expression.
    fn set_param(&mut self, arg: &str) -> Result<(), String> {
        let usage = "use :param NAME EXPRESSION, such as :param ids [1, 2, 3]";
        let arg = arg.trim_start_matches('$');
        let end = arg
            .find(|c: char| c.is_whitespace() || "=:".contains(c))
            .ok_or_else(|| usage.to_string())?;
        let name = arg[..end].to_string();
        let mut expr = arg[end..].trim_start();
        for sep in ["=>", ":", "="] {
            if let Some(e) = expr.strip_prefix(sep) {
                expr = e.trim_start();
                break;
            }
        }
        if name.is_empty() || expr.is_empty() {
            return Err(usage.into());
        }
        let query = format!("RETURN {expr} AS value");
        match self.client.query(self.database.as_deref(), None, &query, &self.params, self.timeout_ms) {
            Ok(mut r) => {
                let value = r.rows.pop().and_then(|mut row| row.pop()).unwrap_or(Json::Null);
                println!("${name} = {}", format::cypher(&value));
                self.params.insert(name, value);
                Ok(())
            }
            Err(e) => {
                self.failed(&e, None);
                Err(String::new())
            }
        }
    }

    fn show_params(&mut self, arg: &str) -> Result<(), String> {
        match arg {
            "" => {
                for (k, v) in &self.params {
                    println!("${k} = {}", format::cypher(v));
                }
                Ok(())
            }
            "clear" => {
                self.params.clear();
                Ok(())
            }
            _ => Err("use :params or :params clear".into()),
        }
    }

    fn set_format(&mut self, arg: &str) -> Result<(), String> {
        if arg.is_empty() {
            println!("{}", self.format.name());
        } else {
            self.format = Format::parse(arg)?;
        }
        Ok(())
    }

    fn set_timeout(&mut self, arg: &str) -> Result<(), String> {
        match arg.to_ascii_lowercase().as_str() {
            "" => match self.timeout_ms {
                Some(ms) => println!("{ms} ms"),
                None => println!("off"),
            },
            "off" | "none" | "0" => self.timeout_ms = None,
            ms => self.timeout_ms = Some(parse_ms(ms)?),
        }
        Ok(())
    }

    fn source(&mut self, arg: &str) -> Result<(), String> {
        if arg.is_empty() {
            return Err("use :source FILE".into());
        }
        if self.depth >= MAX_SOURCE_DEPTH {
            return Err(format!(":source nests more than {MAX_SOURCE_DEPTH} deep"));
        }
        let path = unquote_path(arg);
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        // The file runs as a script: it stops at its first error unless the shell goes on.
        let outer = std::mem::take(&mut self.buffer);
        self.depth += 1;
        let before = self.failures;
        let mut quit = false;
        for line in text.lines() {
            if self.line(line) == Flow::Quit {
                quit = true;
                break;
            }
        }
        if !quit {
            let rest = std::mem::take(&mut self.buffer);
            if !split::is_blank(&rest) {
                self.line(&format!("{};", rest.trim_end()));
            }
        }
        self.depth -= 1;
        self.buffer = outer;
        if self.failures > before { Err(String::new()) } else { Ok(()) }
    }
}

/// Writes a note for the user on standard error, which keeps standard output for results.
fn note(message: &str) {
    eprintln!("{message}");
}

/// Standard output is gone (a closed pipe, such as `| head`): stop quietly.
fn output_failed(e: io::Error) -> ! {
    if e.kind() == io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("writing the result: {e}");
    std::process::exit(1);
}

/// The line of `text` with byte `offset`, and a `^` under it.
fn pointer(text: &str, offset: usize) -> String {
    let offset = offset.min(text.len());
    let offset = (0..=offset).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    let column = text[start..offset].chars().count();
    format!("  {}\n  {}^\n", &text[start..end], " ".repeat(column))
}

/// The database a statement that is only `USE name` switches to.
fn bare_use(text: &str) -> Option<String> {
    let t = text.trim();
    if !t.get(..4)?.eq_ignore_ascii_case("use ") {
        return None;
    }
    let name = t[4..].trim();
    let plain = name.starts_with('`') && name.ends_with('`') && name.len() >= 2
        || (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || "_.-".contains(c)));
    plain.then(|| unquote_name(name))
}

/// A database name, without backticks around it.
fn unquote_name(s: &str) -> String {
    let s = s.trim();
    match s.strip_prefix('`').and_then(|s| s.strip_suffix('`')) {
        Some(inner) => inner.replace("``", "`"),
        None => s.to_string(),
    }
}

fn unquote_path(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

pub fn parse_ms(s: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .ok()
        .filter(|&ms| ms > 0)
        .ok_or_else(|| format!("`{s}` is not a number of milliseconds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_statements() {
        assert_eq!(bare_use("USE movies").as_deref(), Some("movies"));
        assert_eq!(bare_use("use `my db`").as_deref(), Some("my db"));
        assert_eq!(bare_use("USE movies MATCH (n) RETURN n"), None);
        assert_eq!(bare_use("RETURN 1"), None);
    }

    #[test]
    fn error_pointer() {
        assert_eq!(pointer("MATCH (n)\nRETRN n", 10), "  RETRN n\n  ^\n");
        assert_eq!(pointer("RETURN ä x", 10), format!("  RETURN ä x\n  {}^\n", " ".repeat(9)));
    }
}
