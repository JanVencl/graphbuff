//! `graphbuff` — interactive Cypher shell.
//!
//! Usage:
//!   graphbuff                 start an interactive shell
//!   graphbuff script.cypher   run every statement in the file, then exit
//!   graphbuff -i script.cypher  run the file, then continue interactively
//!   graphbuff --demo movies   start with the movies demo graph loaded
//!   graphbuff --data DIR ...  keep databases durable in DIR
//!   graphbuff --data DIR --persistent ...  ... in disk-resident paged storage
//!   graphbuff import --data DIR --nodes FILE ... --relationships FILE ...
//!                             bulk-import CSV files into a new persistent database
//!   graphbuff backup | restore | check ...
//!                             back up, restore or check a database (see --help of each)
//!
//! Statements end with `;`. Shell commands start with `:` (try `:help`).

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Instant;

mod admin;
mod import;

use graphbuff::{DatabaseManager, GraphStore, QueryResult, Session, StorageKind, SyncPolicy};

const HELP: &str = "\
Statements end with ';' and may span several lines.

Commands:
  :help           show this help
  :use <name>     switch to another database (same as USE name;)
  :dbs            list databases (same as SHOW DATABASES;)
  :stats          storage, node / relationship counts, labels and types of the current database
  :checkpoint     flush durable storage and empty its log (hybrid / persistent storage)
  :source <file>  run every statement in a file
  :clear          drop everything in the current database (MATCH (n) DETACH DELETE n)
  :quit           exit (also Ctrl-D / Ctrl-Z)

Databases:
  CREATE DATABASE movies;
  CREATE DATABASE scratch IF NOT EXISTS OPTIONS {storage: 'memory'};
  CREATE OR REPLACE DATABASE movies OPTIONS {storage: 'hybrid', sync: 'os'};
  CREATE DATABASE big OPTIONS {storage: 'persistent', cacheBytes: 268435456};
  SHOW DATABASES;   USE movies;   USE scratch MATCH (n) RETURN count(n);
  DROP DATABASE scratch IF EXISTS;

Examples:
  CREATE (a:Person {name: 'Ann'})-[:KNOWS {since: 2020}]->(b:Person {name: 'Bob'});
  MATCH (p:Person)-[k:KNOWS]->(q) RETURN p.name, k.since, q.name;
  MATCH (p:Person) RETURN count(p) AS people;";

const USAGE: &str = "\
usage: graphbuff [options] [script.cypher ...]

Options:
  -i               run the scripts, then continue interactively
  --data DIR       keep databases durably in DIR (catalog + one folder per database);
                   without it every database lives only in memory
  --persistent     with --data: new databases use disk-resident paged storage
                   (graphs larger than RAM) instead of hybrid in-memory storage
  --no-fsync       with --data: new databases don't fsync every commit (they survive
                   a crash of graphbuff, but not of the machine)
  --demo NAME      start with a demo graph loaded into the default database (unless
                   it already holds data); NAME: movies
  --import-dir DIR where LOAD CSV reads files (default: the current directory)
  --gpu            run graph kernels (multi-hop counts, algo.* procedures) on the GPU
                   (builds with the `gpu` feature)
  -h, --help       show this help

Bulk import of CSV files into a new database: graphbuff import --help
Backup, restore and consistency check: graphbuff backup | restore | check --help";

fn main() {
    let rest = || std::env::args().skip(2).collect();
    match std::env::args().nth(1).as_deref() {
        Some("import") => std::process::exit(import::run(rest())),
        Some("backup") => std::process::exit(admin::backup(rest())),
        Some("restore") => std::process::exit(admin::restore(rest())),
        Some("check") => std::process::exit(admin::check(rest())),
        _ => {}
    }
    let mut interactive_after = false;
    let mut data_dir: Option<String> = None;
    let mut sync = SyncPolicy::EveryCommit;
    let mut persistent = false;
    let mut files: Vec<String> = Vec::new();
    let mut import_dir: Option<String> = None;
    let mut gpu = false;
    let mut demo: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}\n\n{HELP}");
                return;
            }
            "-i" => interactive_after = true,
            "--no-fsync" => sync = SyncPolicy::OsBuffered,
            "--persistent" => persistent = true,
            "--gpu" => gpu = true,
            "--import-dir" => match args.next() {
                Some(dir) => import_dir = Some(dir),
                None => {
                    eprintln!("--import-dir needs a directory

{USAGE}");
                    std::process::exit(2);
                }
            },
            "--demo" => match args.next() {
                Some(name) => {
                    if let Err(e) = graphbuff::demo_script(&name) {
                        eprintln!("{e}

{USAGE}");
                        std::process::exit(2);
                    }
                    demo = Some(name);
                }
                None => {
                    eprintln!("--demo needs a name

{USAGE}");
                    std::process::exit(2);
                }
            },
            "--data" => match args.next() {
                Some(dir) => data_dir = Some(dir),
                None => {
                    eprintln!("--data needs a directory\n\n{USAGE}");
                    std::process::exit(2);
                }
            },
            _ => files.push(arg),
        }
    }
    interactive_after |= files.is_empty();

    let mgr = match data_dir {
        Some(dir) => {
            let mut storage = if persistent {
                StorageKind::persistent()
            } else {
                StorageKind::hybrid()
            };
            if let StorageKind::Hybrid { sync: s, .. } | StorageKind::Persistent { sync: s, .. } =
                &mut storage
            {
                *s = sync;
            }
            DatabaseManager::open_with(dir, storage)
        }
        None => Ok(DatabaseManager::in_memory()),
    };
    let mgr = match mgr {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cannot open data directory: {e}");
            std::process::exit(1);
        }
    };
    mgr.set_import_dir(Some(match import_dir {
        Some(d) => d.into(),
        None => std::env::current_dir().unwrap_or_else(|_| ".".into()),
    }));
    if gpu {
        match graphbuff::gpu_kernels() {
            Ok(k) => mgr.set_kernels(k),
            Err(e) => {
                eprintln!("--gpu: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(name) = &demo {
        match graphbuff::load_demo(&mgr, name) {
            Ok(d) if d.loaded => {
                println!("demo `{name}` loaded: {} nodes, {} relationships", d.nodes, d.relationships)
            }
            Ok(d) => println!("demo `{name}` not loaded: the default database already holds {} nodes", d.nodes),
            Err(e) => {
                eprintln!("cannot load the demo `{name}`: {e}");
                std::process::exit(1);
            }
        }
    }
    let mut session = mgr.session();

    let mut ok = true;
    for f in &files {
        ok &= run_file(&mut session, f);
    }
    if interactive_after {
        repl(&mut session);
    } else if !ok {
        std::process::exit(1);
    }
}

fn run_file(session: &mut Session, path: &str) -> bool {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return false;
        }
    };
    let statements = match graphbuff::cypher::split_statements(&src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    let mut ok = true;
    for stmt in statements {
        ok &= run_statement(session, stmt);
    }
    ok
}

fn run_statement(session: &mut Session, stmt: &str) -> bool {
    let started = Instant::now();
    let before = session.current_database().to_string();
    match session.execute(stmt) {
        Ok(result) => {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            if session.current_database() != before {
                println!("now using database {}", session.current_database());
            } else {
                print_result(&result, ms);
            }
            true
        }
        Err(e) => {
            println!("{}", render_error(stmt, &e));
            false
        }
    }
}

fn print_result(result: &QueryResult, ms: f64) {
    if let Some(plan) = &result.plan {
        print!("{plan}");
    }
    if !result.columns.is_empty() {
        println!("{}", result.to_table());
        let n = result.len();
        print!("{n} row{}", if n == 1 { "" } else { "s" });
        if result.stats.contains_updates() {
            print!("; {}", result.stats);
        }
    } else {
        print!("{}", result.stats);
    }
    println!(" ({ms:.2} ms)");
}

/// Formats an error, pointing at the offending position for syntax errors.
fn render_error(src: &str, e: &graphbuff::Error) -> String {
    let graphbuff::Error::Syntax { message, offset } = e else {
        return e.to_string();
    };
    let offset = (*offset).min(src.len());
    let line_start = src[..offset].rfind('\n').map_or(0, |p| p + 1);
    let line_end = src[offset..].find('\n').map_or(src.len(), |p| offset + p);
    let line_no = src[..offset].matches('\n').count() + 1;
    let col = src[line_start..offset].chars().count();
    format!(
        "SyntaxError: {message} (line {line_no}, column {})\n  {}\n  {}^",
        col + 1,
        &src[line_start..line_end],
        " ".repeat(col)
    )
}

fn print_stats(session: &Session) {
    let db = match session.database() {
        Ok(db) => db,
        Err(e) => return println!("{e}"),
    };
    println!("database:           {}", session.current_database());
    db.read(|s| {
        println!("storage:            {}", s.describe());
        println!("nodes:              {}", s.node_count());
        println!("relationships:      {}", s.relationship_count());
        println!("labels:             {}", s.labels().join(", "));
        println!("relationship types: {}", s.relationship_types().join(", "));
        let indexes: Vec<String> = s
            .indexes()
            .iter()
            .map(|d| format!("{} {}", d.name, d.target()))
            .collect();
        println!("indexes:            {}", indexes.join(", "));
    });
}

fn repl(session: &mut Session) {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        let place = match session.manager().root() {
            Some(dir) => format!("data in {}", dir.display()),
            None => "in-memory only".to_string(),
        };
        println!(
            "graphbuff {} — {place}. Type :help for help.",
            env!("CARGO_PKG_VERSION")
        );
    }
    let mut buffer = String::new();
    let mut lines = stdin.lock().lines();
    loop {
        if interactive {
            let db = session.current_database();
            if buffer.trim().is_empty() {
                print!("{db}> ");
            } else {
                print!("{}...> ", " ".repeat(db.chars().count().saturating_sub(3)));
            }
            let _ = io::stdout().flush();
        }
        let Some(Ok(line)) = lines.next() else {
            if !buffer.trim().is_empty() {
                run_statement(session, buffer.trim());
            }
            break;
        };

        let trimmed = line.trim();
        if buffer.trim().is_empty() && trimmed.starts_with(':') {
            let (cmd, arg) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
            let arg = arg.trim();
            match cmd {
                ":quit" | ":exit" | ":q" => break,
                ":help" | ":h" => println!("{HELP}"),
                ":stats" => print_stats(session),
                ":dbs" => {
                    run_statement(session, "SHOW DATABASES");
                }
                ":use" if !arg.is_empty() => match session.use_database(arg) {
                    Ok(()) => println!("now using database {}", session.current_database()),
                    Err(e) => println!("{e}"),
                },
                ":checkpoint" => match session.database().and_then(|db| db.checkpoint()) {
                    Ok(()) => println!("checkpoint written"),
                    Err(e) => println!("{e}"),
                },
                ":clear" => {
                    run_statement(session, "MATCH (n) DETACH DELETE n");
                }
                ":source" if !arg.is_empty() => {
                    run_file(session, arg);
                }
                _ => println!("unknown command {cmd}; try :help"),
            }
            continue;
        }

        buffer.push_str(&line);
        buffer.push('\n');
        if !trimmed.ends_with(';') {
            continue;
        }
        match graphbuff::cypher::split_statements(&buffer) {
            Ok(statements) => {
                for stmt in statements {
                    run_statement(session, stmt);
                }
                buffer.clear();
            }
            // A `;` inside an unterminated string or comment: keep reading.
            Err(graphbuff::Error::Syntax { message, .. })
                if message.starts_with("unterminated") => {}
            Err(e) => {
                println!("{e}");
                buffer.clear();
            }
        }
    }
}
