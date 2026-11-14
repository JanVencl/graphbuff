//! `graphbuff-server` — serves graphbuff databases over HTTP/JSON and Bolt.

use std::sync::Arc;
use std::time::Duration;

use graphbuff::{DatabaseManager, StorageKind, SyncPolicy};
use graphbuff::cypher::ast::ExistingDatabase;
use graphbuff_server::{Role, Server, ServerConfig, TlsConfig, Users};

const USAGE: &str = "\
usage: graphbuff-server [options]
       graphbuff-server users FILE (list | add NAME [ROLE...] | password NAME
                                    | grant NAME ROLE... | revoke NAME ROLE... | remove NAME)

Options:
  --bind ADDR          address to listen on (default 127.0.0.1:7475; use 0.0.0.0:7475
                       to accept other machines, together with --token)
  --bolt ADDR          also serve the Bolt protocol (Neo4j drivers) on ADDR, such as
                       127.0.0.1:7687; the token, if set, is the password (any user name)
  --data DIR           keep databases durably in DIR; without it they live in memory
  --demo NAME          start with a demo graph loaded into the default database (unless
                       it already holds data); NAME: movies
  --persistent         with --data: new databases use disk-resident paged storage
  --no-fsync           with --data: don't fsync every commit
  --token TOKEN        require `Authorization: Bearer TOKEN` (also: env GRAPHBUFF_TOKEN);
                       the token holder may do anything
  --users FILE         user accounts in FILE (created on the first CREATE USER); clients
                       log in with a user name and password (HTTP Basic, Bolt basic)
  --tls-cert FILE      serve HTTPS and Bolt over TLS with this PEM certificate chain
  --tls-key FILE       ... and this PEM private key (needs the `tls` build feature)
  --tls-allow-plain    with TLS: also accept clients without TLS on the same ports
  --max-connections N  connections served at once (default 256)
  --keep-alive SECS    idle keep-alive timeout (default 30)
  --tx-timeout SECS    roll back a transaction after SECS without a request (default 60)
  --query-timeout MS   stop (and roll back) a query that runs longer than MS milliseconds
                       (default: no limit; a request may ask for less with \"timeoutMs\")
  --import-dir DIR     let LOAD CSV read files in DIR (off by default)
  --backup-dir DIR     let admins back up databases into DIR and restore from it
                       (off by default)
  --gpu                run graph kernels (multi-hop counts, algo.* procedures) on the
                       GPU (needs the `gpu` build feature)
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
  reads new passwords from env GRAPHBUFF_PASSWORD or the first line of standard input.
  Change the file offline only while no server uses it.";

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
        eprintln!("password (one line):");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
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

fn main() {
    if std::env::args().nth(1).as_deref() == Some("users") {
        if let Err(e) = users_command(std::env::args().skip(2)) {
            eprintln!("{e}");
            std::process::exit(2);
        }
        return;
    }
    let mut config = ServerConfig {
        token: std::env::var("GRAPHBUFF_TOKEN").ok().filter(|t| !t.is_empty()),
        ..ServerConfig::default()
    };
    let mut data: Option<String> = None;
    let mut persistent = false;
    let mut sync = SyncPolicy::EveryCommit;
    let mut import_dir: Option<String> = None;
    let mut users_file: Option<String> = None;
    let mut gpu = false;
    let mut demo: Option<String> = None;
    let (mut tls_cert, mut tls_key, mut tls_plain): (Option<String>, Option<String>, bool) = (None, None, false);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| fail(&format!("{name} needs a value")));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--bind" => config.bind = value("--bind"),
            "--data" => data = Some(value("--data")),
            "--demo" => demo = Some(value("--demo")),
            "--bolt" => config.bolt = Some(value("--bolt")),
            "--persistent" => persistent = true,
            "--gpu" => gpu = true,
            "--no-fsync" => sync = SyncPolicy::OsBuffered,
            "--token" => config.token = Some(value("--token")),
            "--import-dir" => import_dir = Some(value("--import-dir")),
            "--backup-dir" => config.backup_dir = Some(value("--backup-dir").into()),
            "--users" => users_file = Some(value("--users")),
            "--tls-cert" => tls_cert = Some(value("--tls-cert")),
            "--tls-key" => tls_key = Some(value("--tls-key")),
            "--tls-allow-plain" => tls_plain = true,
            "--max-connections" => {
                config.max_connections = value("--max-connections")
                    .parse()
                    .unwrap_or_else(|_| fail("--max-connections needs a number"));
            }
            "--keep-alive" => {
                let secs: u64 = value("--keep-alive")
                    .parse()
                    .unwrap_or_else(|_| fail("--keep-alive needs a number of seconds"));
                config.keep_alive = Duration::from_secs(secs);
            }
            "--tx-timeout" => {
                let secs: u64 = value("--tx-timeout")
                    .parse()
                    .ok()
                    .filter(|&s| s > 0)
                    .unwrap_or_else(|| fail("--tx-timeout needs a positive number of seconds"));
                config.transaction_timeout = Duration::from_secs(secs);
            }
            "--query-timeout" => {
                let ms: u64 = value("--query-timeout")
                    .parse()
                    .ok()
                    .filter(|&ms| ms > 0)
                    .unwrap_or_else(|| fail("--query-timeout needs a positive number of milliseconds"));
                config.query_timeout = Some(Duration::from_millis(ms));
            }
            other => fail(&format!("unknown option `{other}`")),
        }
    }

    if let Some(name) = &demo
        && let Err(e) = graphbuff::demo_script(name)
    {
        fail(&e.to_string());
    }
    config.tls = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => Some(TlsConfig {
            allow_plaintext: tls_plain,
            ..TlsConfig::new(cert, key)
        }),
        (None, None) if tls_plain => fail("--tls-allow-plain needs --tls-cert and --tls-key"),
        (None, None) => None,
        _ => fail("TLS needs both --tls-cert and --tls-key"),
    };
    if let Some(file) = &users_file {
        match Users::open(file) {
            Ok(u) => {
                if u.is_empty() && config.token.is_none() {
                    eprintln!(
                        "warning: {file} has no users yet, so nobody can log in; add an admin with `graphbuff-server users {file} add NAME admin`, or start with --token"
                    );
                }
                config.users = Some(Arc::new(u));
            }
            Err(e) => {
                eprintln!("cannot open the users file {file}: {e}");
                std::process::exit(1);
            }
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
        Err(e) => {
            eprintln!("cannot open data directory: {e}");
            std::process::exit(1);
        }
    };

    mgr.set_import_dir(import_dir.map(Into::into));
    let demo = demo.map(|name| match graphbuff::load_demo(&mgr, &name) {
        Ok(d) => (name, d),
        Err(e) => {
            eprintln!("cannot load the demo `{name}`: {e}");
            std::process::exit(1);
        }
    });
    if gpu {
        match graphbuff::gpu_kernels() {
            Ok(k) => mgr.set_kernels(k),
            Err(e) => fail(&format!("--gpu: {e}")),
        }
    }
    let server = match Server::start(mgr, config.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "cannot listen on {}{}: {e}",
                config.bind,
                config.bolt.as_ref().map_or(String::new(), |b| format!(" or {b}"))
            );
            std::process::exit(1);
        }
    };
    let addr = server.local_addr();
    let auth = config.token.is_some() || config.users.is_some();
    if !addr.ip().is_loopback() && !auth {
        eprintln!(
            "warning: listening on {addr} without --token or --users; anyone who can reach this port can read and change every database"
        );
    }
    if let Some(bolt) = server.bolt_addr()
        && !bolt.ip().is_loopback()
        && !auth
    {
        eprintln!(
            "warning: serving Bolt on {bolt} without --token or --users; anyone who can reach this port can use every database"
        );
    }
    let tls = config.tls.is_some();
    if auth && !tls && (!addr.ip().is_loopback() || server.bolt_addr().is_some_and(|b| !b.ip().is_loopback())) {
        eprintln!("warning: without TLS (--tls-cert, --tls-key), passwords and tokens cross the network in clear text");
    }
    let (http, bolt) = if tls { ("https", "bolt+s") } else { ("http", "bolt") };
    println!(
        "graphbuff-server listening on {http}://{addr}{} ({})",
        server.bolt_addr().map_or(String::new(), |b| format!(" and {bolt}://{b}")),
        match &data {
            Some(d) => format!("data in {d}"),
            None => "in-memory databases".into(),
        }
    );
    if let Some((name, d)) = demo {
        if d.loaded {
            println!("demo `{name}` loaded: {} nodes, {} relationships", d.nodes, d.relationships);
        } else {
            println!("demo `{name}` not loaded: the default database already holds {} nodes", d.nodes);
        }
    }
    server.wait();
}
