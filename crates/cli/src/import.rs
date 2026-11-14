//! `graphbuff import`: bulk CSV import into a new persistent database.

use std::path::PathBuf;
use std::time::Instant;

use graphbuff::{
    BulkLoader, DatabaseManager, IdType, ImportFiles, ImportOptions, ImportSummary, PersistentGraph,
    PersistentOptions, StorageKind, SyncPolicy, import_csv, parse_index,
};

const USAGE: &str = "\
usage: graphbuff import (--data DIR [--database NAME] [--overwrite] | --into STORE_DIR)
                        --nodes [LABELS=]FILES ... [--relationships [TYPE=]FILES ...] [options]

Builds a new persistent database from CSV files in the neo4j-admin import format,
writing store records directly (no Cypher, no redo log). The server must not be
running on DIR.

Target:
  --data DIR            data directory of graphbuff / graphbuff-server; the database is
                        added to its catalog (replacing an empty one of the same name)
  --database NAME       database to create (default: the data directory's default)
  --overwrite           replace the database even if it holds data
  --into STORE_DIR      instead: write a bare persistent store into a new directory

Inputs (repeatable; FILES are comma-separated, the first record of the first is the
header; LABELS are colon-separated):
  --nodes [Label1:Label2=]header.csv,part1.csv,...
  --relationships [TYPE=]rels.csv,...

Header fields: name:ID(space)  :ID  :LABEL  :START_ID(space)  :END_ID(space)  :TYPE
  :IGNORE  name  name:int|long|float|double|boolean|string|point|date|localtime|time|
  localdatetime|datetime|duration  and arrays such as name:string[]

Options:
  --delimiter C              field delimiter (default ','; 'TAB' for tabs)
  --array-delimiter C        array element and label separator (default ';')
  --id-type string|integer   how ids are read (default string)
  --skip-bad-relationships   skip relationships with unknown ends instead of failing
  --skip-duplicate-nodes     skip nodes with an id seen before instead of failing
  --index SPEC               build an index: 'Label(prop, ...)' or '[:TYPE](prop, ...)'
  --cache-mb N               page cache while importing (default 512)
  --no-fsync                 the new database does not fsync every commit";

fn files(spec: &str, labels_sep: char) -> ImportFiles {
    let (labels, files) = match spec.split_once('=') {
        Some((l, f)) => (l.split(labels_sep).filter(|s| !s.is_empty()).map(String::from).collect(), f),
        None => (Vec::new(), spec),
    };
    ImportFiles {
        labels,
        files: files.split(',').filter(|s| !s.is_empty()).map(PathBuf::from).collect(),
    }
}

fn one_char(flag: &str, v: &str) -> Result<char, String> {
    match v {
        "TAB" | "\\t" => Ok('\t'),
        _ => {
            let mut c = v.chars();
            match (c.next(), c.next()) {
                (Some(ch), None) => Ok(ch),
                _ => Err(format!("{flag} needs a single character")),
            }
        }
    }
}

struct Args {
    data: Option<String>,
    into: Option<String>,
    database: Option<String>,
    overwrite: bool,
    sync: SyncPolicy,
    opts: ImportOptions,
}

fn parse(args: Vec<String>) -> Result<Option<Args>, String> {
    let mut a = Args {
        data: None,
        into: None,
        database: None,
        overwrite: false,
        sync: SyncPolicy::EveryCommit,
        opts: ImportOptions::default(),
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        // `--flag=value` and `--flag value`.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> Result<String, String> {
            inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--data" => a.data = Some(value()?),
            "--into" => a.into = Some(value()?),
            "--database" => a.database = Some(value()?),
            "--overwrite" => a.overwrite = true,
            "--no-fsync" => a.sync = SyncPolicy::OsBuffered,
            "--nodes" => a.opts.nodes.push(files(&value()?, ':')),
            "--relationships" => a.opts.relationships.push(files(&value()?, ':')),
            "--delimiter" => {
                let c = one_char(&flag, &value()?)?;
                a.opts.delimiter = u8::try_from(c).map_err(|_| "--delimiter must be an ASCII character".to_string())?;
            }
            "--array-delimiter" => a.opts.array_delimiter = one_char(&flag, &value()?)?,
            "--id-type" => {
                a.opts.id_type = match value()?.to_ascii_lowercase().as_str() {
                    "string" => IdType::String,
                    "integer" | "actual" => IdType::Integer,
                    other => return Err(format!("unknown --id-type `{other}`")),
                }
            }
            "--skip-bad-relationships" => a.opts.skip_bad_relationships = true,
            "--skip-duplicate-nodes" => a.opts.skip_duplicate_nodes = true,
            "--index" => a.opts.indexes.push(parse_index(&value()?).map_err(|e| e.to_string())?),
            "--cache-mb" => {
                let mb: u64 = value()?.parse().map_err(|_| "--cache-mb needs a number".to_string())?;
                a.opts.cache_bytes = mb << 20;
            }
            _ => return Err(format!("unknown option `{arg}`")),
        }
    }
    match (&a.data, &a.into) {
        (None, None) => return Err("give --data DIR or --into STORE_DIR".into()),
        (Some(_), Some(_)) => return Err("give only one of --data and --into".into()),
        (None, Some(_)) if a.database.is_some() || a.overwrite => {
            return Err("--database and --overwrite go with --data".into());
        }
        _ => {}
    }
    if a.opts.nodes.is_empty() {
        return Err("give at least one --nodes input".into());
    }
    Ok(Some(a))
}

fn into_store(dir: &str, a: &Args) -> graphbuff::Result<ImportSummary> {
    let opts = PersistentOptions::new(dir).sync(a.sync).cache_bytes(a.opts.cache_bytes);
    let mut loader: BulkLoader = PersistentGraph::bulk_loader(opts)?;
    let summary = import_csv(&mut loader, &a.opts)?;
    loader.finish()?;
    Ok(summary)
}

fn into_database(data: &str, a: &Args) -> graphbuff::Result<(String, ImportSummary)> {
    let mgr = DatabaseManager::open(data)?;
    let name = a.database.clone().unwrap_or_else(|| mgr.default_database());
    let mut storage = StorageKind::persistent();
    if let StorageKind::Persistent { sync, .. } = &mut storage {
        *sync = a.sync;
    }
    let summary = mgr.import_database(&name, Some(storage), &a.opts, a.overwrite)?;
    Ok((name, summary))
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: Vec<String>) -> i32 {
    let a = match parse(args) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return 2;
        }
    };
    let started = Instant::now();
    let result = match (&a.data, &a.into) {
        (Some(data), _) => into_database(data, &a).map(|(name, s)| (format!("database `{name}`"), s)),
        (_, Some(dir)) => into_store(dir, &a).map(|s| (dir.clone(), s)),
        _ => unreachable!("checked by parse"),
    };
    match result {
        Ok((target, s)) => {
            let secs = started.elapsed().as_secs_f64();
            println!(
                "imported {} nodes, {} relationships, {} properties into {target} in {secs:.2} s",
                s.nodes, s.relationships, s.properties
            );
            if s.indexes > 0 {
                println!("built {} indexes", s.indexes);
            }
            if s.skipped_nodes > 0 || s.skipped_relationships > 0 {
                println!(
                    "skipped {} duplicate nodes and {} bad relationships",
                    s.skipped_nodes, s.skipped_relationships
                );
            }
            0
        }
        Err(e) => {
            eprintln!("import failed: {e}");
            1
        }
    }
}
