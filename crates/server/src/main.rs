//! `graphbuff-server` — serves graphbuff databases over HTTP/JSON and Bolt.

mod prompt;
mod settings;
mod signal;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::{DatabaseManager, StorageKind, SyncPolicy};
use graphbuff::cypher::ast::ExistingDatabase;
use settings::Settings;
use graphbuff_server::{Level, Log, LogConfig, Role, Server, ServerConfig, TlsConfig, Users};

const USAGE: &str = "\
usage: graphbuff-server [options]
       graphbuff-server users FILE (list | add NAME [ROLE...] | password NAME
                                    | grant NAME ROLE... | revoke NAME ROLE... | remove NAME)

Options:
  --config FILE        read settings from FILE (also: env GRAPHBUFF_CONFIG; `-` for none);
                       default, created with every setting commented out if missing:
                         Linux    $XDG_CONFIG_HOME/graphbuff/graphbuff.conf
                                  (~/.config/graphbuff/graphbuff.conf),
                                  /etc/graphbuff/graphbuff.conf when run as root
                         macOS    ~/Library/Application Support/graphbuff/graphbuff.conf
                         Windows  %LOCALAPPDATA%\\graphbuff\\graphbuff.conf
                       One `key = value` per line, keys are the options below without
                       `--` (`bind = 0.0.0.0:7475`, `in-memory = true`). The command line
                       overrides the environment, which overrides the file.
  --bind ADDR          address to listen on (default 127.0.0.1:7475; use 0.0.0.0:7475
                       to accept other machines, together with --token)
  --bolt ADDR          also serve the Bolt protocol (Neo4j drivers) on ADDR, such as
                       127.0.0.1:7687; the token, if set, is the password (any user name)
  --data DIR           keep databases durably in DIR (also: env GRAPHBUFF_DATA); default:
                         Linux    $XDG_DATA_HOME/graphbuff (~/.local/share/graphbuff),
                                  /var/lib/graphbuff when run as root
                         macOS    ~/Library/Application Support/graphbuff
                         Windows  %LOCALAPPDATA%\\graphbuff
  --in-memory          keep databases in memory only; they are lost when the server stops
  --demo NAME          start with a demo graph loaded into the default database (unless
                       it already holds data); NAME: movies
  --persistent         with --data: new databases use disk-resident paged storage
  --no-fsync           with --data: don't fsync every commit
  --token TOKEN        require `Authorization: Bearer TOKEN` (also: env GRAPHBUFF_TOKEN);
                       the token holder may do anything
  --users FILE         user accounts in FILE (created on the first CREATE USER); clients
                       log in with a user name and password (HTTP Basic, Bolt basic)
                       (also: env GRAPHBUFF_USERS; `-` for none)
  --tls-cert FILE      serve HTTPS and Bolt over TLS with this PEM certificate chain
  --tls-key FILE       ... and this PEM private key (needs the `tls` build feature)
  --tls-allow-plain    with TLS: also accept clients without TLS on the same ports
  --max-connections N  connections served at once (default 256)
  --keep-alive SECS    idle keep-alive timeout (default 30)
  --tx-timeout SECS    roll back a transaction after SECS without a request (default 60)
  --query-timeout MS   stop (and roll back) a query that runs longer than MS milliseconds
                       (default: no limit; a request may ask for less with \"timeoutMs\")
  --shutdown-timeout SECS
                       on SIGTERM or Ctrl-C, how long running queries may take to finish
                       before they are cancelled (default 5); then durable databases are
                       checkpointed and the server exits
  --import-dir DIR     let LOAD CSV read files in DIR (off by default)
  --backup-dir DIR     let admins back up databases into DIR and restore from it
                       (off by default)
  --gpu                run graph kernels (multi-hop counts, algo.* procedures) on the
                       GPU (needs the `gpu` build feature)
  --log-dir DIR        write graphbuff.log and query.log in DIR (also: env
                       GRAPHBUFF_LOG_DIR); `-` for standard error; default: DATA/logs,
                       or standard error with --in-memory
  --log-level LEVEL    error, warn, info (default) or debug (also every connection);
                       also: env GRAPHBUFF_LOG_LEVEL
  --slow-query MS      log queries that take at least MS milliseconds to query.log
  --test-procedures    for compatibility tests only: admins may add procedures that
                       answer from a fixed table (POST /db/{name}/test-procedure)
  -h, --help           show this help

Endpoints:
  GET  /health
  GET  /db
  POST /db/{name}/query   body: {\"query\": \"MATCH (n) RETURN n\", \"params\": {...}, \"timeoutMs\": 5000}
  POST /query             the same, against the default database
  POST /db/{name}/tx      begin a transaction; answers {\"transaction\": {\"id\": ...}}
  POST /db/{name}/tx/{id} run a statement in it (same body as /query)
  POST /db/{name}/tx/{id}/commit
  DELETE /db/{name}/tx/{id}
                          roll back
  /tx, /tx/{id}, ...      the same, against the default database
  POST /db/{name}/backup  online backup into the backup directory; body: {\"backup\": \"nightly\"}
  POST /db/{name}/restore create the database from a backup; body: {\"backup\": \"nightly\",
                          \"overwrite\": false}
  POST /db/{name}/check   consistency check of a persistent database, online
                          (backup, restore and check need the admin role)

User accounts (--users):
  Roles: reader (read queries), editor (also writes, indexes, constraints), admin (also
  databases and users). Manage them with Cypher as an admin (CREATE USER alice SET
  PASSWORD 'pw', GRANT ROLE editor TO alice, SHOW USERS, ALTER CURRENT USER SET PASSWORD
  FROM 'old' TO 'new', ...), or offline with `graphbuff-server users FILE ...`, which
  reads new passwords from env GRAPHBUFF_PASSWORD, else asks for them without echo on a
  terminal, else reads the first line of standard input. Change the file offline only
  while no server uses it.

  The first administrator, at start-up: when the users file has no accounts yet, the
  server creates GRAPHBUFF_ADMIN_USER (default: admin) with the admin role and the
  password in env GRAPHBUFF_ADMIN_PASSWORD, or in the file named by
  GRAPHBUFF_ADMIN_PASSWORD_FILE (a Docker or Compose secret; one trailing newline is
  ignored). Once the file has accounts, these variables are ignored.";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}\n\n{USAGE}");
    std::process::exit(2);
}

/// `graphbuff-server users FILE …`: manages the accounts in FILE offline.
fn users_command(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let file = args.next().ok_or("users needs a FILE")?;
    let users = Users::open(&file).map_err(|e| format!("cannot open {file}: {e}"))?;
    let command = args.next().ok_or("users FILE needs a command: list, add, password, grant, revoke or remove")?;
    let rest: Vec<String> = args.collect();
    let name = || rest.first().cloned().ok_or(format!("users FILE {command} needs a user NAME"));
    let roles = || -> Result<Vec<Role>, String> {
        rest[1..].iter().map(|r| Role::parse(r).map_err(|e| e.to_string())).collect()
    };
    let password = || -> Result<String, String> {
        if let Ok(p) = std::env::var("GRAPHBUFF_PASSWORD") {
            return Ok(p);
        }
        prompt::new_password().map_err(|e| format!("cannot read the password: {e}"))
    };
    let err = |e: graphbuff::Error| e.to_string();
    match command.as_str() {
        "list" => {
            for (name, roles) in users.list() {
                let roles: Vec<&str> = roles.iter().map(|r| r.name()).collect();
                println!("{name}\t{}", roles.join(","));
            }
        }
        "add" => {
            let name = name()?;
            let roles = roles()?;
            users.create(&name, &password()?, &roles, ExistingDatabase::Fail).map_err(err)?;
            println!("added user {name}");
        }
        "password" => {
            let name = name()?;
            users.set_password(&name, &password()?).map_err(err)?;
            println!("changed the password of {name}");
        }
        "grant" | "revoke" => {
            let name = name()?;
            let roles = roles()?;
            if roles.is_empty() {
                return Err(format!("users FILE {command} needs at least one ROLE"));
            }
            for role in roles {
                if command == "grant" {
                    users.grant(&name, role).map_err(err)?;
                } else {
                    users.revoke(&name, role).map_err(err)?;
                }
            }
        }
        "remove" => {
            let name = name()?;
            if !users.drop_user(&name).map_err(err)? {
                return Err(format!("user `{name}` does not exist"));
            }
            println!("removed user {name}");
        }
        other => return Err(format!("unknown users command `{other}`")),
    }
    Ok(())
}

/// The first administrator from the environment: GRAPHBUFF_ADMIN_USER (default `admin`)
/// and GRAPHBUFF_ADMIN_PASSWORD or GRAPHBUFF_ADMIN_PASSWORD_FILE; `None` without either.
fn first_admin() -> Result<Option<(String, String)>, String> {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let password = match (env("GRAPHBUFF_ADMIN_PASSWORD"), env("GRAPHBUFF_ADMIN_PASSWORD_FILE")) {
        (Some(_), Some(_)) => {
            return Err("set GRAPHBUFF_ADMIN_PASSWORD or GRAPHBUFF_ADMIN_PASSWORD_FILE, not both".into());
        }
        (Some(p), None) => p,
        (None, Some(file)) => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| format!("cannot read GRAPHBUFF_ADMIN_PASSWORD_FILE {file}: {e}"))?;
            let text = text.strip_suffix('\n').unwrap_or(&text);
            text.strip_suffix('\r').unwrap_or(text).to_string()
        }
        (None, None) => {
            if env("GRAPHBUFF_ADMIN_USER").is_some() {
                return Err("GRAPHBUFF_ADMIN_USER needs GRAPHBUFF_ADMIN_PASSWORD or GRAPHBUFF_ADMIN_PASSWORD_FILE".into());
            }
            return Ok(None);
        }
    };
    Ok(Some((env("GRAPHBUFF_ADMIN_USER").unwrap_or_else(|| "admin".into()), password)))
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("users") {
        if let Err(e) = users_command(std::env::args().skip(2)) {
            eprintln!("{e}");
            std::process::exit(2);
        }
        return;
    }
    let command_line = Settings::command_line(std::env::args().skip(1)).unwrap_or_else(|e| fail(&e));
    if command_line.help {
        println!("{USAGE}");
        return;
    }
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let mut settings = Settings::default();
    // The config file: --config, else GRAPHBUFF_CONFIG, else the default one. It is
    // created (every setting commented out) when it does not exist.
    let config_file = match command_line.config.clone().or_else(|| env("GRAPHBUFF_CONFIG")) {
        Some(f) if f == "-" => None,
        Some(f) => Some(PathBuf::from(f)),
        None => settings::default_config_file(),
    };
    let config_file: Option<PathBuf> = config_file.and_then(|f| match std::fs::read_to_string(&f) {
            Ok(text) => {
                settings.apply_file(&f, &text).unwrap_or_else(|e| fail(&e));
                Some(f)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let written = f.parent().map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&f, settings::TEMPLATE));
                match written {
                    Ok(()) => Some(f),
                    Err(e) => {
                        eprintln!("warning: cannot create the config file {}: {e}", f.display());
                        None
                    }
                }
            }
            Err(e) => fail(&format!("cannot read the config file {}: {e}", f.display())),
        });
    settings.apply_env().unwrap_or_else(|e| fail(&e));
    settings.apply_command_line(command_line.settings).unwrap_or_else(|e| fail(&e));

    let get = |key: &str| settings.get(key).map(str::to_string);
    let number = |key: &str, what: &str, positive: bool| -> Option<u64> {
        settings.get(key).map(|v| {
            v.parse::<u64>()
                .ok()
                .filter(|&n| !positive || n > 0)
                .unwrap_or_else(|| fail(&format!("--{key} needs a {}number of {what}", if positive { "positive " } else { "" })))
        })
    };
    let mut config = ServerConfig {
        token: get("token"),
        bolt: get("bolt"),
        backup_dir: get("backup-dir").map(Into::into),
        test_procedures: settings.flag("test-procedures"),
        ..ServerConfig::default()
    };
    if let Some(bind) = get("bind") {
        config.bind = bind;
    }
    if let Some(n) = settings.get("max-connections") {
        config.max_connections = n.parse().unwrap_or_else(|_| fail("--max-connections needs a number"));
    }
    if let Some(s) = number("keep-alive", "seconds", false) {
        config.keep_alive = Duration::from_secs(s);
    }
    if let Some(s) = number("tx-timeout", "seconds", true) {
        config.transaction_timeout = Duration::from_secs(s);
    }
    if let Some(ms) = number("query-timeout", "milliseconds", true) {
        config.query_timeout = Some(Duration::from_millis(ms));
    }
    if let Some(s) = number("shutdown-timeout", "seconds", false) {
        config.shutdown_grace = Duration::from_secs(s);
    }
    let slow_query = number("slow-query", "milliseconds", false).map(Duration::from_millis);
    let in_memory = settings.flag("in-memory");
    let persistent = settings.flag("persistent");
    let sync = if settings.flag("no-fsync") { SyncPolicy::OsBuffered } else { SyncPolicy::EveryCommit };
    let import_dir = get("import-dir");
    let users_file = get("users").filter(|f| f != "-");
    let gpu = settings.flag("gpu");
    let demo = get("demo");
    let (tls_cert, tls_key, tls_plain) = (get("tls-cert"), get("tls-key"), settings.flag("tls-allow-plain"));

    if let Some(name) = &demo
        && let Err(e) = graphbuff::demo_script(name)
    {
        fail(&e.to_string());
    }

    let data: Option<PathBuf> = if in_memory {
        None
    } else {
        Some(match get("data") {
            Some(d) => d.into(),
            None => settings::default_data_dir().unwrap_or_else(|| {
                fail("no default data directory (HOME or LOCALAPPDATA is not set); use --data DIR or --in-memory")
            }),
        })
    };
    let level = match settings.get("log-level") {
        Some(l) => Level::parse(&l).unwrap_or_else(|| fail("--log-level needs error, warn, info or debug")),
        None => Level::Info,
    };
    let log_dir = match get("log-dir") {
        Some(d) if d == "-" => None,
        Some(d) => Some(PathBuf::from(d)),
        None => data.as_ref().map(|d| d.join("logs")),
    };
    let log = match Log::open(LogConfig {
        dir: log_dir.clone(),
        level,
        slow_query,
        ..LogConfig::default()
    }) {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("cannot open the log directory {}: {e}", log_dir.unwrap_or_default().display());
            std::process::exit(1);
        }
    };
    config.log = Some(Arc::clone(&log));
    // Problems go to the console and, when the log is a file, to the log too.
    let warn = |msg: &str| {
        if log.dir().is_some() {
            eprintln!("warning: {msg}");
        }
        log.warn(msg);
    };
    let fatal = |msg: &str| -> ! {
        if log.dir().is_some() {
            eprintln!("{msg}");
        }
        log.error(msg);
        std::process::exit(1);
    };
    config.tls = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => Some(TlsConfig {
            allow_plaintext: tls_plain,
            ..TlsConfig::new(cert, key)
        }),
        (None, None) if tls_plain => fail("--tls-allow-plain needs --tls-cert and --tls-key"),
        (None, None) => None,
        _ => fail("TLS needs both --tls-cert and --tls-key"),
    };
    let admin = first_admin().unwrap_or_else(|e| fatal(&e));
    if admin.is_some() && users_file.is_none() {
        fatal("GRAPHBUFF_ADMIN_PASSWORD needs a users file (--users FILE or env GRAPHBUFF_USERS)");
    }
    if let Some(file) = &users_file {
        match Users::open(file) {
            Ok(u) => {
                match &admin {
                    Some((name, password)) if u.is_empty() => {
                        if let Err(e) = u.create(name, password, &[Role::Admin], ExistingDatabase::Fail) {
                            fatal(&format!("cannot create the administrator `{name}` in {file}: {e}"));
                        }
                        log.info(format!("created the administrator `{name}` in {file} (GRAPHBUFF_ADMIN_PASSWORD)"));
                    }
                    Some(_) => log.info(format!(
                        "{file} already has accounts; GRAPHBUFF_ADMIN_USER and GRAPHBUFF_ADMIN_PASSWORD are ignored"
                    )),
                    None => {}
                }
                if u.is_empty() && config.token.is_none() {
                    warn(&format!(
                        "{file} has no users yet, so nobody can log in; add an admin with `graphbuff-server users {file} add NAME admin`, set GRAPHBUFF_ADMIN_PASSWORD, or start with --token"
                    ));
                }
                config.users = Some(Arc::new(u));
            }
            Err(e) => fatal(&format!("cannot open the users file {file}: {e}")),
        }
    }

    let mgr = match &data {
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
        Ok(m) => Arc::new(m),
        Err(e) => fatal(&format!(
            "cannot open the data directory {}: {e}",
            data.as_deref().unwrap_or_else(|| std::path::Path::new("")).display()
        )),
    };

    mgr.set_import_dir(import_dir.map(Into::into));
    let demo = demo.map(|name| match graphbuff::load_demo(&mgr, &name) {
        Ok(d) => (name, d),
        Err(e) => fatal(&format!("cannot load the demo `{name}`: {e}")),
    });
    if gpu {
        match graphbuff::gpu_kernels() {
            Ok(k) => mgr.set_kernels(k),
            Err(e) => fail(&format!("--gpu: {e}")),
        }
    }
    let server = match Server::start(Arc::clone(&mgr), config.clone()) {
        Ok(s) => s,
        Err(e) => fatal(&format!(
            "cannot listen on {}{}: {e}",
            config.bind,
            config.bolt.as_ref().map_or(String::new(), |b| format!(" or {b}"))
        )),
    };
    let addr = server.local_addr();
    let auth = config.token.is_some() || config.users.is_some();
    if !addr.ip().is_loopback() && !auth {
        warn(&format!(
            "listening on {addr} without --token or --users; anyone who can reach this port can read and change every database"
        ));
    }
    if let Some(bolt) = server.bolt_addr()
        && !bolt.ip().is_loopback()
        && !auth
    {
        warn(&format!(
            "serving Bolt on {bolt} without --token or --users; anyone who can reach this port can use every database"
        ));
    }
    let tls = config.tls.is_some();
    if auth && !tls && (!addr.ip().is_loopback() || server.bolt_addr().is_some_and(|b| !b.ip().is_loopback())) {
        warn("without TLS (--tls-cert, --tls-key), passwords and tokens cross the network in clear text");
    }
    let (http, bolt) = if tls { ("https", "bolt+s") } else { ("http", "bolt") };
    let listening = format!(
        "graphbuff-server {} listening on {http}://{addr}{} ({})",
        env!("CARGO_PKG_VERSION"),
        server.bolt_addr().map_or(String::new(), |b| format!(" and {bolt}://{b}")),
        match &data {
            Some(d) => format!("data in {}", d.display()),
            None => "in-memory databases, lost when the server stops".into(),
        }
    );
    println!("{listening}");
    log.info(&listening);
    let fsync = if sync == SyncPolicy::EveryCommit { "fsync every commit" } else { "no fsync" };
    log.info(format_args!(
        "config: {}; new databases: {}; log: {}; slow-query log: {}; logins: {}",
        config_file.as_ref().map_or("none".into(), |f| f.display().to_string()),
        match (&data, persistent) {
            (None, _) => "in memory".to_string(),
            (Some(_), true) => format!("persistent, {fsync}"),
            (Some(_), false) => format!("hybrid, {fsync}"),
        },
        log.dir().map_or("standard error".into(), |d| d.display().to_string()),
        log.slow_query().map_or("off".into(), |d| format!("{} ms", d.as_millis())),
        match (&config.users, &config.token) {
            (Some(_), _) => "user accounts",
            (None, Some(_)) => "token",
            (None, None) => "none needed",
        }
    ));
    if let Some((name, d)) = demo {
        if d.loaded {
            println!("demo `{name}` loaded: {} nodes, {} relationships", d.nodes, d.relationships);
            log.info(format_args!("demo `{name}` loaded"));
        } else {
            println!("demo `{name}` not loaded: the default database already holds {} nodes", d.nodes);
        }
    }
    signal::install();
    while !signal::requested() {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Stop: no new connections, roll back open transactions, let running queries finish
    // (or cancel them after the grace period), then checkpoint durable databases.
    let started = Instant::now();
    println!("stopping");
    log.info("stop requested; stopping");
    server.shutdown();
    let mut failed = false;
    for (name, result) in mgr.checkpoint_all() {
        if let Err(e) = result {
            failed = true;
            let msg = format!("checkpoint of database `{name}` failed (its log is kept and replayed on the next start): {e}");
            if log.dir().is_some() {
                eprintln!("{msg}");
            }
            log.error(msg);
        }
    }
    drop(mgr); // syncs the logs and releases the data directory
    let stopped = format!("stopped in {} ms", started.elapsed().as_millis());
    println!("{stopped}");
    log.info(&stopped);
    signal::done();
    std::process::exit(i32::from(failed));
}
