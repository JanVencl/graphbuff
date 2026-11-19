//! `graphbuff-cli`: a command-line client for graphbuff-server's HTTP/JSON API.
//!
//! Runs Cypher given on the command line, in files or on standard input, or at an
//! interactive prompt when standard input is a terminal.

mod client;
mod editor;
mod format;
mod highlight;
mod prompt;
mod shell;
mod split;
mod style;
mod tls;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use graphbuff::json::{self, Json};

use client::{Auth, Client, Url};
use editor::{Editor, Input};
use format::Format;
use shell::{Flow, Shell};
use style::{Color, paint};

const DEFAULT_URL: &str = "http://127.0.0.1:7475";

const USAGE: &str = "\
graphbuff-cli: run Cypher against a graphbuff server

Usage: graphbuff-cli [OPTIONS] [STATEMENTS]...

With STATEMENTS (joined with spaces) or --file, runs them and exits. Otherwise reads
standard input: an interactive prompt on a terminal, else a script. Statements end with
`;`; lines starting with `:` are commands (`:help` lists them).

At the prompt, Cypher is highlighted as it is typed. Keys: Left/Right, Ctrl-Left/Right
(words), Home/End (Ctrl-A/E), Up/Down (history, kept in ~/.graphbuff_history or the file
in env GRAPHBUFF_HISTORY, empty for none), Ctrl-U/K/W (delete to start / to end / word),
Ctrl-L (clear), Ctrl-C (drop the statement), Ctrl-D (leave, on an empty line).

Options:
  -a, --address URL       the server [env GRAPHBUFF_URL] (default http://127.0.0.1:7475)
  -u, --user NAME         log in as NAME [env GRAPHBUFF_USER]
  -p, --password PASS     NAME's password [env GRAPHBUFF_PASSWORD]; asked for if missing
      --token TOKEN       log in with the server's token [env GRAPHBUFF_TOKEN]
  -d, --database NAME     run against database NAME (default: the server's default)
  -f, --file FILE         run the statements in FILE (`-` for standard input); repeatable
  -P, --param NAME=VALUE  set parameter $NAME; VALUE is JSON, else a string; repeatable
      --format FORMAT     table, plain (tab-separated), csv or json (one result per line);
                          default: table on a terminal, else plain
      --timeout MS        stop each statement after MS milliseconds
      --continue          go on after a statement fails (scripts stop at the first error)
      --cacert FILE       trust the CA certificates in FILE (PEM) for https:// URLs
      --color WHEN        colors: auto (on a terminal unless NO_COLOR is set), always, never
      --no-color          the same as --color never
  -h, --help              this help
  -V, --version           the version

Exit status: 0 when everything ran, 1 when a statement or command failed, 2 for
wrong usage or when the server cannot be reached or refuses the login.

Examples:
  graphbuff-cli -u admin \"MATCH (n) RETURN count(n);\"
  graphbuff-cli -d movies --format csv -f report.cypher > report.csv
  graphbuff-cli -P name=Ann \"MATCH (p:Person {name: \\$name}) RETURN p\"";

#[derive(Default)]
struct Args {
    url: Option<String>,
    user: Option<String>,
    password: Option<String>,
    token: Option<String>,
    database: Option<String>,
    files: Vec<String>,
    params: Vec<(String, Json)>,
    format: Option<Format>,
    timeout_ms: Option<u64>,
    keep_going: bool,
    cacert: Option<PathBuf>,
    color: Option<style::When>,
    statements: Vec<String>,
    help: bool,
    version: bool,
}

impl Args {
    fn parse(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut a = Args::default();
        let mut only_positional = false;
        while let Some(arg) = it.next() {
            if only_positional || !arg.starts_with('-') || arg == "-" {
                a.statements.push(arg);
                continue;
            }
            if arg == "--" {
                only_positional = true;
                continue;
            }
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };
            let mut value = |name: &str| -> Result<String, String> {
                inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{name} needs a value"))
            };
            match flag.as_str() {
                "-h" | "--help" => a.help = true,
                "-V" | "--version" => a.version = true,
                "-a" | "--address" | "--url" => a.url = Some(value(&flag)?),
                "-u" | "--user" => a.user = Some(value(&flag)?),
                "-p" | "--password" => a.password = Some(value(&flag)?),
                "--token" => a.token = Some(value(&flag)?),
                "-d" | "--database" => a.database = Some(value(&flag)?),
                "-f" | "--file" => a.files.push(value(&flag)?),
                "-P" | "--param" => a.params.push(parse_param(&value(&flag)?)?),
                "--format" => a.format = Some(Format::parse(&value(&flag)?)?),
                "--timeout" => a.timeout_ms = Some(shell::parse_ms(&value(&flag)?)?),
                "--continue" => a.keep_going = true,
                "--cacert" => a.cacert = Some(PathBuf::from(value(&flag)?)),
                "--color" | "--colour" => a.color = Some(style::When::parse(&value(&flag)?)?),
                "--no-color" | "--no-colour" => a.color = Some(style::When::Never),
                _ => return Err(format!("unknown option `{arg}`")),
            }
            if inline.is_some() && matches!(flag.as_str(), "--help" | "--version" | "--continue" | "--no-color" | "--no-colour") {
                return Err(format!("{flag} takes no value"));
            }
        }
        Ok(a)
    }
}

/// `name=value`: the value as JSON if it is JSON, else as a string.
fn parse_param(s: &str) -> Result<(String, Json), String> {
    let (name, value) = s.split_once('=').ok_or_else(|| format!("`{s}`: use NAME=VALUE"))?;
    let name = name.trim().trim_start_matches('$');
    if name.is_empty() {
        return Err(format!("`{s}`: the parameter has no name"));
    }
    let value = json::parse(value).unwrap_or_else(|_| Json::String(value.to_string()));
    Ok((name.to_string(), value))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(m) => {
            eprintln!("graphbuff-cli: {m}\nTry `graphbuff-cli --help`.");
            return 2;
        }
    };
    if args.help {
        println!("{USAGE}");
        return 0;
    }
    if args.version {
        println!("graphbuff-cli {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    match start(args) {
        Ok(code) => code,
        Err(m) => {
            eprintln!("graphbuff-cli: {m}");
            2
        }
    }
}

fn start(args: Args) -> Result<i32, String> {
    style::init(args.color.unwrap_or(style::When::Auto));
    let url_text = args.url.or_else(|| env("GRAPHBUFF_URL")).unwrap_or_else(|| DEFAULT_URL.into());
    let url = Url::parse(&url_text)?;
    let stdin_script = args.files.iter().any(|f| f == "-");
    let interactive = args.statements.is_empty() && args.files.is_empty() && io::stdin().is_terminal();

    let user = args.user.or_else(|| env("GRAPHBUFF_USER"));
    let token = args.token.or_else(|| env("GRAPHBUFF_TOKEN"));
    let auth = match (user, token) {
        (Some(_), Some(_)) => return Err("log in with a user or with the token, not both".into()),
        (Some(name), None) => {
            let password = match args.password.or_else(|| env("GRAPHBUFF_PASSWORD")) {
                Some(p) => p,
                None if prompt::interactive() && !stdin_script => {
                    prompt::password(&format!("password for {name}: ")).map_err(|e| e.to_string())?
                }
                None => return Err(format!("give {name}'s password with --password or GRAPHBUFF_PASSWORD")),
            };
            Auth::User { name, password }
        }
        (None, Some(token)) => Auth::Token(token),
        (None, None) => Auth::None,
    };

    let connector = tls::Connector::new(&url, args.cacert)?;
    let mut client = Client::new(url, auth, connector);

    let format = args.format.unwrap_or(if interactive || io::stdout().is_terminal() {
        Format::Table
    } else {
        Format::Plain
    });
    if interactive {
        connect_interactively(&mut client)?;
    }
    let mut shell = Shell::new(client, format, interactive);
    shell.database = args.database;
    shell.params = args.params.into_iter().collect();
    shell.timeout_ms = args.timeout_ms;
    if args.keep_going {
        shell.stop_on_error = false;
    }

    if interactive {
        repl(&mut shell);
        return Ok(if shell.failures > 0 { 1 } else { 0 });
    }

    // Statements on the command line, then files, else standard input.
    let mut flow = Flow::Go;
    if !args.statements.is_empty() {
        let text = args.statements.join(" ");
        for line in text.lines() {
            flow = shell.line(line);
            if flow == Flow::Quit {
                break;
            }
        }
    }
    let files = if args.statements.is_empty() && args.files.is_empty() { vec!["-".to_string()] } else { args.files };
    for file in &files {
        if flow == Flow::Quit {
            break;
        }
        flow = run_file(&mut shell, file)?;
    }
    if flow == Flow::Go {
        shell.finish();
    } else if let Some(tx) = shell.tx.take() {
        let _ = shell.client.rollback(&tx);
        eprintln!("the open transaction was rolled back");
    }
    Ok(if shell.failures > 0 { 1 } else { 0 })
}

/// Feeds `file` (`-`: standard input) to the shell line by line, ending its last
/// statement.
fn run_file(shell: &mut Shell, file: &str) -> Result<Flow, String> {
    let reader: Box<dyn BufRead> = if file == "-" {
        Box::new(io::stdin().lock())
    } else {
        let f = std::fs::File::open(file).map_err(|e| format!("{file}: {e}"))?;
        Box::new(io::BufReader::new(f))
    };
    for line in reader.lines() {
        let line = line.map_err(|e| format!("reading {file}: {e}"))?;
        if shell.line(&line) == Flow::Quit {
            return Ok(Flow::Quit);
        }
    }
    // A file's last statement needs no `;`.
    if shell.pending() && shell.line(";") == Flow::Quit {
        return Ok(Flow::Quit);
    }
    Ok(Flow::Go)
}

/// Checks the server and the login before the prompt; asks for a user and password
/// when the server wants a login and none was given.
fn connect_interactively(client: &mut Client) -> Result<(), String> {
    let mut tries = 0;
    loop {
        match client.databases() {
            Ok(_) => break,
            Err(e) if e.is_unauthorized() && tries < 3 => {
                if !matches!(client.auth(), Auth::None) {
                    eprintln!("{}", paint(style::err(), Color::Error, &e.to_string()));
                }
                tries += 1;
                let name = prompt::line("user: ").map_err(|e| e.to_string())?;
                let password = prompt::password("password: ").map_err(|e| e.to_string())?;
                client.set_auth(Auth::User { name, password });
            }
            Err(e) => return Err(format!("{}: {e}", client.url())),
        }
    }
    let who = match client.auth() {
        Auth::None => String::new(),
        Auth::Token(_) => " with the token".into(),
        Auth::User { name, .. } => format!(" as {name}"),
    };
    let on = style::err();
    eprintln!(
        "Connected to {}{who}. {}",
        paint(on, Color::Good, &client.url().to_string()),
        paint(on, Color::Dim, "Type :help for help, :quit to leave.")
    );
    Ok(())
}

fn repl(shell: &mut Shell) {
    let color = style::out();
    let mut editor = Editor::new(color);
    loop {
        let prompt = shell.prompt(color);
        let line = if io::stdout().is_terminal() {
            editor.read_line(&prompt, shell.pending_text())
        } else {
            plain_line(&prompt)
        };
        match line {
            Ok(Input::Line(line)) => {
                if shell.line(&line) == Flow::Quit {
                    break;
                }
            }
            Ok(Input::Interrupted) => shell.cancel(),
            Ok(Input::Eof) => break,
            Err(e) => {
                eprintln!("reading standard input: {e}");
                break;
            }
        }
    }
    shell.finish();
}

/// A line read the plain way (standard output is not a terminal).
fn plain_line(prompt: &str) -> io::Result<Input> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        println!();
        return Ok(Input::Eof);
    }
    Ok(Input::Line(line.trim_end_matches(['\r', '\n']).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Result<Args, String> {
        Args::parse(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn options() {
        let a = args(&["-u", "ann", "--database=movies", "-P", "n=1", "-P", "s=x y", "RETURN", "$n;"]).unwrap();
        assert_eq!(a.user.as_deref(), Some("ann"));
        assert_eq!(a.database.as_deref(), Some("movies"));
        assert_eq!(a.params, [("n".into(), Json::Int(1)), ("s".into(), Json::String("x y".into()))]);
        assert_eq!(a.statements, ["RETURN", "$n;"]);
        assert!(args(&["--bogus"]).is_err());
        assert!(args(&["-d"]).is_err());
        assert!(args(&["--format", "xml"]).is_err());
        assert_eq!(args(&["--", "-x"]).unwrap().statements, ["-x"]);
    }
}
