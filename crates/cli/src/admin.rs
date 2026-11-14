//! `graphbuff backup`, `graphbuff restore` and `graphbuff check`: offline maintenance of
//! a data directory or a bare store. A running server does the same online over HTTP.

use std::path::{Path, PathBuf};
use std::time::Instant;

use graphbuff::{DatabaseManager, GraphStore, PersistentGraph, PersistentOptions};

const BACKUP_USAGE: &str = "\
usage: graphbuff backup (--data DIR [--database NAME] | --store STORE_DIR) --to BACKUP_DIR

Writes a copy of a database into BACKUP_DIR, a new or empty directory. A persistent
database backs up as a persistent store, the others as hybrid stores; either opens
directly (graphbuff restore, or as a store). The server must not be running on DIR: to
back up a running server, use its POST /db/{name}/backup endpoint.

  --data DIR        data directory of graphbuff / graphbuff-server
  --database NAME   database to back up (default: the data directory's default)
  --store DIR       instead: a bare persistent store (written by graphbuff import --into)
  --to BACKUP_DIR   where to write the backup";

const RESTORE_USAGE: &str = "\
usage: graphbuff restore --from BACKUP_DIR --data DIR [--database NAME] [--overwrite]

Creates a database from a backup. The backup is copied (and a persistent one checked
first), so it stays usable. An existing database is replaced only if it is empty or
--overwrite is given. The server must not be running on DIR: to restore into a running
server, use its POST /db/{name}/restore endpoint.

  --from BACKUP_DIR  the backup (graphbuff backup, or the server's backup endpoint)
  --data DIR         data directory of graphbuff / graphbuff-server
  --database NAME    database to create (default: the data directory's default)
  --overwrite        replace the database even if it holds data";

const CHECK_USAGE: &str = "\
usage: graphbuff check (--data DIR [--database NAME] | --store STORE_DIR)

Checks a persistent database for consistency: records, adjacency lists, counts, free
space, indexes and constraints. Exits with 1 if it finds errors. The server must not be
running on DIR: to check a running server, use its POST /db/{name}/check endpoint.

  --data DIR        data directory of graphbuff / graphbuff-server
  --database NAME   database to check (default: the data directory's default)
  --store DIR       instead: a bare persistent store";

#[derive(Default)]
struct Args {
    data: Option<String>,
    database: Option<String>,
    store: Option<String>,
    to: Option<String>,
    from: Option<String>,
    overwrite: bool,
}

/// Parses `--flag value` / `--flag=value` options; `allowed` lists the valid flags.
fn parse(args: Vec<String>, allowed: &[&str]) -> Result<Option<Args>, String> {
    let mut a = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        if flag == "-h" || flag == "--help" {
            return Ok(None);
        }
        if !allowed.contains(&flag.as_str()) {
            return Err(format!("unknown option `{arg}`"));
        }
        let mut value = || -> Result<String, String> {
            inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--data" => a.data = Some(value()?),
            "--database" => a.database = Some(value()?),
            "--store" => a.store = Some(value()?),
            "--to" => a.to = Some(value()?),
            "--from" => a.from = Some(value()?),
            "--overwrite" => a.overwrite = true,
            _ => unreachable!("checked against `allowed`"),
        }
    }
    Ok(Some(a))
}

/// The source of `backup` / `check`: a database in a data directory, or a bare store.
#[allow(clippy::large_enum_variant)] // one short-lived value
enum Source {
    Data { mgr: DatabaseManager, name: String },
    Store(PersistentGraph),
}

fn source(a: &Args) -> Result<Source, String> {
    match (&a.data, &a.store) {
        (Some(data), None) => {
            if !Path::new(data).join("catalog").is_file() {
                return Err(format!("{data} is not a graphbuff data directory (it has no catalog)"));
            }
            let mgr = DatabaseManager::open(data).map_err(|e| format!("cannot open {data}: {e}"))?;
            let name = a.database.clone().unwrap_or_else(|| mgr.default_database());
            Ok(Source::Data { mgr, name })
        }
        (None, Some(dir)) => {
            if a.database.is_some() {
                return Err("--database goes with --data".into());
            }
            if !Path::new(dir).join("nodes.db").is_file() {
                return Err(format!("{dir} is not a persistent store (it has no nodes.db)"));
            }
            PersistentGraph::open(PersistentOptions::new(dir))
                .map(Source::Store)
                .map_err(|e| format!("cannot open {dir}: {e}"))
        }
        (None, None) => Err("give --data DIR or --store STORE_DIR".into()),
        (Some(_), Some(_)) => Err("give only one of --data and --store".into()),
    }
}

fn usage_error(e: &str, usage: &str) -> i32 {
    eprintln!("{e}\n\n{usage}");
    2
}

/// `graphbuff backup`; returns the exit code.
pub fn backup(args: Vec<String>) -> i32 {
    let a = match parse(args, &["--data", "--database", "--store", "--to"]) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{BACKUP_USAGE}");
            return 0;
        }
        Err(e) => return usage_error(&e, BACKUP_USAGE),
    };
    let Some(to) = a.to.clone() else {
        return usage_error("give --to BACKUP_DIR", BACKUP_USAGE);
    };
    let src = match source(&a) {
        Ok(s) => s,
        Err(e) => return usage_error(&e, BACKUP_USAGE),
    };
    let started = Instant::now();
    let to = PathBuf::from(to);
    let (what, result) = match &src {
        Source::Data { mgr, name } => (format!("database `{name}`"), mgr.backup_database(name, &to)),
        Source::Store(g) => (a.store.clone().unwrap_or_default(), g.backup(&to)),
    };
    match result {
        Ok(info) => {
            println!(
                "backed up {what} ({} nodes, {} relationships, {:.1} MiB, {} format) to {} in {:.2} s",
                info.nodes,
                info.relationships,
                info.bytes as f64 / (1 << 20) as f64,
                info.format,
                info.dir.display(),
                started.elapsed().as_secs_f64()
            );
            0
        }
        Err(e) => {
            eprintln!("backup failed: {e}");
            1
        }
    }
}

/// `graphbuff restore`; returns the exit code.
pub fn restore(args: Vec<String>) -> i32 {
    let a = match parse(args, &["--data", "--database", "--from", "--overwrite"]) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{RESTORE_USAGE}");
            return 0;
        }
        Err(e) => return usage_error(&e, RESTORE_USAGE),
    };
    let (Some(from), Some(data)) = (&a.from, &a.data) else {
        return usage_error("give --from BACKUP_DIR and --data DIR", RESTORE_USAGE);
    };
    let started = Instant::now();
    let result = DatabaseManager::open(data).and_then(|mgr| {
        let name = a.database.clone().unwrap_or_else(|| mgr.default_database());
        mgr.restore_database(&name, Path::new(from), a.overwrite)
    });
    match result {
        Ok(info) => {
            println!(
                "restored database `{}` ({} storage, {} nodes, {} relationships) from {from} in {:.2} s",
                info.name,
                info.storage.name(),
                info.nodes,
                info.relationships,
                started.elapsed().as_secs_f64()
            );
            0
        }
        Err(e) => {
            eprintln!("restore failed: {e}");
            1
        }
    }
}

/// `graphbuff check`; returns the exit code (1 if the store is inconsistent).
pub fn check(args: Vec<String>) -> i32 {
    let a = match parse(args, &["--data", "--database", "--store"]) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{CHECK_USAGE}");
            return 0;
        }
        Err(e) => return usage_error(&e, CHECK_USAGE),
    };
    let src = match source(&a) {
        Ok(s) => s,
        Err(e) => return usage_error(&e, CHECK_USAGE),
    };
    let started = Instant::now();
    let result = match &src {
        Source::Data { mgr, name } => mgr.check_database(name),
        Source::Store(g) => g.check(),
    };
    match result {
        Ok(report) => {
            println!("{report}");
            println!("({:.2} s)", started.elapsed().as_secs_f64());
            if report.is_consistent() { 0 } else { 1 }
        }
        Err(e) => {
            eprintln!("check failed: {e}");
            1
        }
    }
}
