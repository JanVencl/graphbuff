//! `graphbuff-server` — serves graphbuff databases over HTTP/JSON and Bolt.

mod prompt;
mod settings;
mod signal;

/// Counts the memory each thread holds, for the per-query memory limit.
#[global_allocator]
static ALLOCATOR: graphbuff::accounting::CountingAllocator = graphbuff::accounting::CountingAllocator;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::{DatabaseManager, StorageKind, SyncPolicy};
use graphbuff::cypher::ast::ExistingDatabase;
use settings::{Config, LogDir, Storage};
use graphbuff_server::{
    DEFAULT_TOKEN_LIFETIME, Log, LogConfig, LoginLimits, McpConfig, Role, Server, ServerConfig, TlsConfig, TokenAccess, Users,
};

const USAGE: &str = "\
usage: graphbuff-server [--config FILE]
       graphbuff-server users [--config FILE]
                        (list | add NAME [ROLE...] | password NAME
                         | grant NAME ROLE... | revoke NAME ROLE... | remove NAME)
       graphbuff-server tokens [--config FILE]
                        (list [USER] | add USER NAME [--write] [--database DB]...
                         [--days N | --never] | remove USER NAME)
       graphbuff-server admin [--config FILE] (init [NAME] | reset-password NAME)
       graphbuff-server mcp [--url URL] [--cacert FILE]
                        (MCP over stdin and stdout, to a running server; see mcp --help)

Settings:
  Every setting is in the config file, graphbuff.toml (TOML). The server reads:
    1. FILE, with --config FILE (also: env GRAPHBUFF_CONFIG; `-` for the built-in defaults)
    2. config/graphbuff.toml in the program's folder (the release archives ship it)
    3. the per-user (system) one:
         Linux    $XDG_CONFIG_HOME/graphbuff/graphbuff.toml
                  (~/.config/graphbuff/graphbuff.toml),
                  /etc/graphbuff/graphbuff.toml when run as root
         macOS    ~/Library/Application Support/graphbuff/graphbuff.toml
         Windows  %LOCALAPPDATA%\\graphbuff\\graphbuff.toml
  Without any of them it creates config/graphbuff.toml next to the program (or, if that
  folder cannot be written, the per-user one) with every setting and its default. The
  file describes each setting; sections: [server] (addresses, connections), [storage]
  (mode, data folder, fsync, import and backup folders), [security] (token, TLS),
  [queries] (timeouts, GPU), [logs], [embedding] (text embedding models, in a build
  with the `embed` feature). Relative paths are relative to the file's folder.
  Env GRAPHBUFF_TOKEN overrides security.token.

  Without storage.data, databases are kept in:
    Linux    $XDG_DATA_HOME/graphbuff (~/.local/share/graphbuff),
             /var/lib/graphbuff when run as root
    macOS    ~/Library/Application Support/graphbuff
    Windows  %LOCALAPPDATA%\\graphbuff

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

User accounts:
  Clients log in with a user name and password (HTTP Basic, Bolt basic), or with the
  token. The accounts are kept in the system database of the data directory (in memory
  with storage.mode = \"memory\").

  Roles: reader (read queries), editor (also writes, indexes, constraints), admin (also
  databases and users); reader and editor also on one database (GRANT ROLE editor ON
  DATABASE movies TO alice). Manage them with Cypher as an admin (CREATE USER alice SET
  PASSWORD 's3cret-pw', GRANT ROLE editor TO alice, SHOW USERS, ALTER CURRENT USER SET PASSWORD
  FROM 'old' TO 'new', ...), or offline with `graphbuff-server users ...` on the data
  directory (the same config file as the server, or --config FILE), only while
  no server uses it. New passwords come from env GRAPHBUFF_PASSWORD, else are asked for
  without echo on a terminal, else read from the first line of standard input.

  API tokens, for programs: a user's token logs in as `Authorization: Bearer gbt_...`
  (Bolt: bearer), and may do what its user may, up to its own cap: only reading, unless it
  was made with ACCESS WRITE (--write); on every database, unless it names some; never
  administration. It expires after 90 days unless made otherwise. Make them with Cypher
  (CREATE TOKEN laptop [ACCESS WRITE] [ON DATABASE movies] [EXPIRES IN 30 DAYS], SHOW
  TOKENS, DROP TOKEN laptop; admins also FOR USER alice), or offline with
  `graphbuff-server tokens ...`, which prints the new token on standard output. The token
  is shown only once.

  The first administrator, at start-up: when no user has the admin role, the server
  creates GRAPHBUFF_ADMIN_USER (default: admin) with the admin role and the password in
  env GRAPHBUFF_ADMIN_PASSWORD, or in the file named by GRAPHBUFF_ADMIN_PASSWORD_FILE (a
  Docker or Compose secret; one trailing newline is ignored). Without them it asks on
  the terminal; without a terminal, or with storage.mode = \"memory\", it does not start.
  With a token (security.token)
  it starts anyway (the token holder can create users). Once there is an administrator,
  these variables are ignored, and no change may leave none.

  Offline, while no server uses the data directory: `graphbuff-server admin init [NAME]`
  creates the first administrator (NAME, else GRAPHBUFF_ADMIN_USER, else asked for on a
  terminal, else admin; the password from GRAPHBUFF_ADMIN_PASSWORD or _FILE, else asked
  for like the users command), and refuses when there is one already. Lost the
  administrator's password? `graphbuff-server admin reset-password NAME` sets a new one.

  Passwords need at least 8 characters and must differ from the user name.";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}\n\n{USAGE}");
    std::process::exit(2);
}

/// The settings of the old command-line options, for the error that names their place in
/// the config file.
const OLD_OPTIONS: &[(&str, &str)] = &[
    ("bind", "bind in [server]"),
    ("bolt", "bolt in [server]"),
    ("max-connections", "max_connections in [server]"),
    ("keep-alive", "keep_alive_seconds in [server]"),
    ("shutdown-timeout", "shutdown_timeout_seconds in [server]"),
    ("data", "data in [storage]"),
    ("in-memory", "mode = \"memory\" in [storage]"),
    ("persistent", "mode = \"persistent\" in [storage]"),
    ("no-fsync", "fsync = false in [storage]"),
    ("import-dir", "import_dir in [storage]"),
    ("backup-dir", "backup_dir in [storage]"),
    ("demo", "demo in [storage]"),
    ("token", "token in [security] (or env GRAPHBUFF_TOKEN)"),
    ("tls-cert", "tls_cert in [security]"),
    ("tls-key", "tls_key in [security]"),
    ("tls-allow-plain", "tls_allow_plain in [security]"),
    ("tx-timeout", "transaction_timeout_seconds in [queries]"),
    ("query-timeout", "query_timeout_ms in [queries]"),
    ("gpu", "gpu in [queries]"),
    ("test-procedures", "test_procedures in [queries]"),
    ("log-dir", "dir in [logs]"),
    ("log-level", "level in [logs]"),
    ("slow-query", "slow_query_ms in [logs]"),
];

/// Reads `--config FILE` (and `-h` / `--help`) from the front of `args`: the file, if
/// given, and whether help was asked for. Any other option is an error, which for an old
/// command-line option names the setting that took its place.
fn config_option(args: &mut std::iter::Peekable<impl Iterator<Item = String>>) -> Result<(Option<String>, bool), String> {
    let (mut config, mut help) = (None, false);
    while let Some(arg) = args.next_if(|a| a.starts_with('-')) {
        match arg.as_str() {
            "-h" | "--help" => help = true,
            "--config" => config = Some(args.next().ok_or("--config needs a FILE")?),
            other => {
                let name = other.trim_start_matches('-');
                return Err(match OLD_OPTIONS.iter().find(|(o, _)| *o == name) {
                    Some((_, setting)) => format!(
                        "`{other}` is gone: graphbuff-server takes its settings from the config file \
                         (graphbuff.toml); set {setting}"
                    ),
                    None => format!("unknown option `{other}`"),
                });
            }
        }
    }
    Ok((config, help))
}

/// The settings of the config file: `--config` (`explicit`), else env GRAPHBUFF_CONFIG,
/// else the one next to the program, else the per-user one; and the file read. With
/// `create`, a missing file is created from the template, next to the program if that
/// folder can be written, else in the per-user place.
fn load_config(explicit: Option<String>, create: bool) -> (Config, Option<PathBuf>) {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let read = |f: &PathBuf| -> Config {
        let text = std::fs::read_to_string(f).unwrap_or_else(|e| fail(&format!("cannot read the config file {}: {e}", f.display())));
        Config::parse(f, &text).unwrap_or_else(|e| fail(&e))
    };
    let (mut config, file) = match explicit.or_else(|| env("GRAPHBUFF_CONFIG")) {
        Some(f) if f == "-" => (Config::default(), None),
        Some(f) => {
            let f = PathBuf::from(f);
            (read(&f), Some(f))
        }
        None => {
            let candidates = [settings::program_config_file(), settings::user_config_file()];
            match candidates.iter().flatten().find(|f| f.is_file()) {
                Some(f) => (read(f), Some(f.clone())),
                None => {
                    if let Some(old) = settings::config_dir().map(|d| d.join("graphbuff.conf")).filter(|f| f.is_file()) {
                        fail(&format!(
                            "{} is no longer read: graphbuff-server now reads graphbuff.toml (TOML, with \
                             sections). Create it with `graphbuff-server` (after moving the old file away) and \
                             copy your settings over",
                            old.display()
                        ));
                    }
                    let created = if create { create_config(&candidates) } else { None };
                    match created {
                        Some(f) => (read(&f), Some(f)),
                        None => (Config::default(), None),
                    }
                }
            }
        }
    };
    config.apply_env();
    (config, file)
}

/// Writes the template to the first of `candidates` (next to the program, per-user) that
/// can be written; the file written.
fn create_config(candidates: &[Option<PathBuf>; 2]) -> Option<PathBuf> {
    for (i, f) in candidates.iter().enumerate() {
        let Some(f) = f else { continue };
        let written = f
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(f, settings::template(i == 0)));
        match written {
            Ok(()) => {
                eprintln!("created the config file {}", f.display());
                return Some(f.clone());
            }
            Err(e) if i == 0 => {
                let _ = e; // next to the program may be read-only; try the per-user place
            }
            Err(e) => eprintln!("warning: cannot create the config file {}: {e}", f.display()),
        }
    }
    None
}

/// The data directory, or `None` in memory mode.
fn data_dir(config: &Config) -> Option<PathBuf> {
    config.data_dir().unwrap_or_else(|e| fail(&e))
}

/// The storage of new databases in the data directory.
fn default_storage(config: &Config) -> StorageKind {
    let mut storage = if config.storage == Storage::Persistent { StorageKind::persistent() } else { StorageKind::hybrid() };
    if !config.fsync
        && let StorageKind::Hybrid { sync, .. } | StorageKind::Persistent { sync, .. } = &mut storage
    {
        *sync = SyncPolicy::OsBuffered;
    }
    storage
}

/// The accounts in the system database of the data directory, for the offline command
/// `what` with a `[--config FILE]` option, while no server uses it; also the
/// command word and the arguments after it. The manager holds the data directory's lock.
fn offline_users(
    what: &str,
    commands: &str,
    args: impl Iterator<Item = String>,
) -> Result<(DatabaseManager, Users, String, Vec<String>), String> {
    let mut args = args.peekable();
    let (file, _) = config_option(&mut args)?;
    let (config, _) = load_config(file, false);
    let command = args.next().ok_or(format!("{what} needs a command: {commands}"))?;
    let data = data_dir(&config).ok_or(format!(
        "{what} works on a data folder, but the config file says storage.mode = \"memory\""
    ))?;
    let mgr = DatabaseManager::open_with(&data, default_storage(&config))
        .map_err(|e| format!("cannot open the data directory {} (is a server using it?): {e}", data.display()))?;
    let system = mgr.system_database().ok_or("this build has no system database")?;
    let users = Users::system(system).map_err(|e| e.to_string())?;
    Ok((mgr, users, command, args.collect()))
}

/// `graphbuff-server users [--config FILE] …`: manages the accounts in the
/// system database of the data directory, while no server uses it.
fn users_command(args: impl Iterator<Item = String>) -> Result<(), String> {
    let (_mgr, users, command, rest) =
        offline_users("users", "list, add, password, grant, revoke or remove", args)?;
    let name = || rest.first().cloned().ok_or(format!("users {command} needs a user NAME"));
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
                return Err(format!("users {command} needs at least one ROLE"));
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

/// `graphbuff-server tokens [--config FILE] …`: manages the API tokens in the system
/// database of the data directory, while no server uses it.
fn tokens_command(args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mgr, users, command, rest) = offline_users("tokens", "list, add or remove", args)?;
    let err = |e: graphbuff::Error| e.to_string();
    match command.as_str() {
        "list" => {
            let user = match rest.as_slice() {
                [] => None,
                [user] => Some(user.as_str()),
                _ => return Err("tokens list takes at most a USER".into()),
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            for t in users.tokens(user) {
                let databases: Vec<&str> = t.databases.iter().map(String::as_str).collect();
                let databases = if databases.is_empty() { "*".to_string() } else { databases.join(",") };
                let expires = match t.expires {
                    None => "never expires".to_string(),
                    Some(e) if e <= now => "expired".to_string(),
                    Some(e) => format!("expires in {} h", (e - now) / 3_600_000),
                };
                println!("{}\t{}\t{}\t{databases}\t{expires}", t.user, t.name, t.access.name());
            }
        }
        "add" => {
            let (mut names, mut access, mut databases) = (Vec::new(), TokenAccess::Read, Vec::new());
            let mut lifetime = Some(DEFAULT_TOKEN_LIFETIME);
            let mut it = rest.into_iter();
            while let Some(arg) = it.next() {
                let mut value = || it.next().ok_or(format!("{arg} needs a value"));
                match arg.as_str() {
                    "--write" => access = TokenAccess::Write,
                    "--never" => lifetime = None,
                    "--days" => {
                        let days = value()?.parse().ok().filter(|d| (1..=3650).contains(d));
                        let days: u64 = days.ok_or("--days needs a number from 1 to 3650")?;
                        lifetime = Some(Duration::from_secs(days * 24 * 3600));
                    }
                    "--database" => {
                        let db = value()?;
                        mgr.database(&db).map_err(err)?;
                        databases.push(mgr.resolve_name(&db));
                    }
                    option if option.starts_with("--") => return Err(format!("unknown tokens add option {option}")),
                    _ => names.push(arg),
                }
            }
            let [user, name] = names.as_slice() else {
                return Err("tokens add needs a USER and a NAME".into());
            };
            let (token, _) = users.create_token(user, name, access, &databases, lifetime).map_err(err)?;
            eprintln!("added token {name} of {user} ({}); it is not shown again", access.name());
            println!("{token}");
        }
        "remove" => {
            let [user, name] = rest.as_slice() else {
                return Err("tokens remove needs a USER and a NAME".into());
            };
            if !users.drop_token(user, name).map_err(err)? {
                return Err(format!("user `{user}` has no token `{name}`"));
            }
            println!("removed token {name} of {user}");
        }
        other => return Err(format!("unknown tokens command `{other}`")),
    }
    Ok(())
}

/// The names of the users with the admin role.
fn admins(users: &Users) -> Vec<String> {
    users.list().into_iter().filter(|(_, roles)| roles.contains(&Role::Admin)).map(|(n, _)| n).collect()
}

/// `graphbuff-server admin [--config FILE] (init [NAME] | reset-password NAME)`:
/// creates the first administrator, or sets a new password for a user, offline.
fn admin_command(args: impl Iterator<Item = String>) -> Result<(), String> {
    let (_mgr, users, command, rest) = offline_users("admin", "init or reset-password", args)?;
    let err = |e: graphbuff::Error| e.to_string();
    match (command.as_str(), rest.as_slice()) {
        ("init", [] | [_]) => {
            if users.has_admin() {
                return Err(format!(
                    "there is an administrator already ({}); to get in again, use \
                     `graphbuff-server admin reset-password NAME`",
                    admins(&users).join(", ")
                ));
            }
            let from_env = first_admin()?;
            let name = rest.first().cloned().or_else(|| from_env.as_ref().map(|(n, _)| n.clone()));
            let existing = name.as_deref().unwrap_or("admin");
            if users.roles(existing).is_some() {
                return Err(format!(
                    "user `{existing}` exists already; give it the admin role with \
                     `graphbuff-server users grant {existing} admin`"
                ));
            }
            let name = match from_env {
                Some((_, password)) => {
                    let name = name.unwrap_or_else(|| "admin".into());
                    users.create(&name, &password, &[Role::Admin], ExistingDatabase::Fail).map_err(err)?;
                    name
                }
                None if prompt::interactive() => ask_for_admin(&users, name)?,
                None => {
                    let name = name.unwrap_or_else(|| "admin".into());
                    let password = prompt::new_password().map_err(|e| format!("cannot read the password: {e}"))?;
                    users.create(&name, &password, &[Role::Admin], ExistingDatabase::Fail).map_err(err)?;
                    name
                }
            };
            println!("created the administrator {name}");
        }
        ("init", _) => return Err("admin init takes at most one NAME".into()),
        ("reset-password", [name]) => {
            if users.roles(name).is_none() {
                return Err(format!(
                    "user `{name}` does not exist; administrators: {}",
                    match admins(&users) {
                        a if a.is_empty() => "none (create one with `graphbuff-server admin init`)".into(),
                        a => a.join(", "),
                    }
                ));
            }
            let password = match std::env::var("GRAPHBUFF_PASSWORD") {
                Ok(p) => p,
                Err(_) => prompt::new_password().map_err(|e| format!("cannot read the password: {e}"))?,
            };
            users.set_password(name, &password).map_err(err)?;
            println!("reset the password of {name}");
        }
        ("reset-password", _) => return Err("admin reset-password needs one user NAME".into()),
        (other, _) => return Err(format!("unknown admin command `{other}`")),
    }
    Ok(())
}

/// Asks on the terminal for the first administrator's name (unless given) and password,
/// and creates the account; a refused password is asked for again, at most three times.
/// Returns the name.
fn ask_for_admin(users: &Users, name: Option<String>) -> Result<String, String> {
    eprintln!("There is no administrator yet. Create one now (Ctrl-C to stop).");
    let name = match name {
        Some(n) => n,
        None => {
            let n = prompt::line("administrator name [admin]: ").map_err(|e| e.to_string())?;
            if n.trim().is_empty() { "admin".to_string() } else { n.trim().to_string() }
        }
    };
    graphbuff_server::users::check_name(&name).map_err(|e| e.to_string())?;
    eprintln!("password (at least {} characters, not the name):", graphbuff_server::MIN_PASSWORD_LENGTH);
    let mut last = String::new();
    for _ in 0..3 {
        let created = prompt::new_password()
            .map_err(|e| e.to_string())
            .and_then(|pw| users.create(&name, &pw, &[Role::Admin], ExistingDatabase::Fail).map_err(|e| e.to_string()));
        match created {
            Ok(_) => {
                eprintln!("created the administrator `{name}`");
                return Ok(name);
            }
            Err(e) => {
                eprintln!("{e}");
                last = e;
            }
        }
    }
    Err(last)
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

/// An offline command (`users`, `admin`) on its arguments.
type OfflineCommand = fn(std::iter::Skip<std::env::Args>) -> Result<(), String>;

/// Sets up the models of `[embedding]` (`vector.embed`). Without a model folder or models
/// there is nothing to do; bad models are skipped with a warning.
#[cfg(feature = "embed")]
fn start_embedding(
    mgr: &DatabaseManager,
    e: &settings::Embedding,
    config_file: Option<&std::path::Path>,
    log: &Arc<Log>,
    warn: &dyn Fn(&str),
) -> Result<(), String> {
    use graphbuff_embed::{DeviceKind, EmbedderConfig, LocalEmbedder, Registry};

    let Some(dir) = e.model_dir(config_file) else {
        log.info("embedding: no model folder (no config file, no embedding.model_dir); vector.embed() is off");
        return Ok(());
    };
    let registry = match Registry::scan(&dir) {
        Ok(r) => r,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if e.model_dir.is_some() {
                return Err(format!("the model folder {} does not exist (embedding.model_dir)", dir.display()));
            }
            log.info(format_args!("embedding: no model folder {}; vector.embed() is off", dir.display()));
            return Ok(());
        }
        Err(err) => return Err(format!("cannot read the model folder {}: {err}", dir.display())),
    };
    for s in registry.skipped() {
        warn(&format!("embedding model `{}` in {} is skipped: {}", s.name, dir.display(), s.reason));
    }
    if registry.is_empty() {
        if let Some(m) = &e.default_model {
            return Err(format!("embedding.default_model `{m}` is not installed in {}", dir.display()));
        }
        log.info(format_args!("embedding: no models in {}; vector.embed() is off", dir.display()));
        return Ok(());
    }
    let config = EmbedderConfig {
        default_model: e.default_model.clone(),
        batch_size: e.batch_size,
        threads: e.threads,
        max_loaded_models: e.max_loaded_models,
        device: match e.device {
            settings::EmbedDevice::Cpu => DeviceKind::Cpu,
            settings::EmbedDevice::Gpu => DeviceKind::Gpu,
        },
    };
    let names: Vec<String> = registry.models().map(|m| format!("{} ({} dimensions)", m.name, m.manifest.dimensions)).collect();
    let load_log = Arc::clone(log);
    let embedder = LocalEmbedder::new(registry, config)?.on_load(move |m| {
        load_log.info(format_args!(
            "loaded embedding model `{}` ({} dimensions, {:.1} MiB of weights) in {} ms",
            m.name,
            m.dimensions,
            m.bytes as f64 / (1 << 20) as f64,
            m.took.as_millis()
        ));
    });
    log.info(format_args!(
        "embedding models in {}: {}; default: {}; device: {}",
        dir.display(),
        names.join(", "),
        embedder.default_model().unwrap_or("none (queries name the model)"),
        if e.device == settings::EmbedDevice::Gpu { "gpu" } else { "cpu" }
    ));
    mgr.set_embedder(Arc::new(embedder));
    Ok(())
}

/// Without the `embed` feature: `[embedding]` settings have no effect, so say so.
#[cfg(not(feature = "embed"))]
fn start_embedding(
    _mgr: &DatabaseManager,
    e: &settings::Embedding,
    _config_file: Option<&std::path::Path>,
    _log: &Arc<Log>,
    warn: &dyn Fn(&str),
) -> Result<(), String> {
    if *e != settings::Embedding::default() {
        warn("[embedding] is set, but this build has no embedding models (build with the `embed` feature)");
    }
    Ok(())
}

fn main() {
    let offline: Option<OfflineCommand> =
        match std::env::args().nth(1).as_deref() {
            Some("users") => Some(users_command),
            Some("tokens") => Some(tokens_command),
            Some("admin") => Some(admin_command),
            _ => None,
        };
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        if let Err(e) = graphbuff_server::mcp_bridge(std::env::args().skip(2)) {
            eprintln!("graphbuff-server mcp: {e}");
            std::process::exit(2);
        }
        return;
    }
    if let Some(command) = offline {
        if let Err(e) = command(std::env::args().skip(2)) {
            eprintln!("{e}");
            std::process::exit(2);
        }
        return;
    }
    let mut args = std::env::args().skip(1).peekable();
    let (file, help) = config_option(&mut args).unwrap_or_else(|e| fail(&e));
    if help {
        println!("{USAGE}");
        return;
    }
    if let Some(extra) = args.next() {
        fail(&format!("unexpected `{extra}`"));
    }
    let (settings, config_file) = load_config(file, true);

    let mut config = ServerConfig {
        token: settings.token.clone(),
        bolt: settings.bolt.clone(),
        backup_dir: settings.backup_dir.clone(),
        test_procedures: settings.test_procedures,
        cors: settings.cors_origins.clone(),
        settings: settings.listing(),
        mcp: settings.mcp_bind.clone().map(|bind| McpConfig {
            bind,
            allowed_origins: settings.mcp_origins.clone(),
            max_concurrent_per_user: settings.mcp_max_concurrent,
            calls_per_minute: settings.mcp_calls_per_minute,
            max_rows: settings.mcp_max_rows,
            max_rows_limit: settings.mcp_max_rows_limit,
            max_result_bytes: settings.mcp_max_result_kb << 10,
            query_timeout: settings.mcp_query_timeout,
        }),
        login_limits: LoginLimits {
            max_failures: settings.max_failed_logins,
            lockout: settings.login_lockout,
        },
        ..ServerConfig::default()
    };
    if let Some(bind) = &settings.bind {
        config.bind = bind.clone();
    }
    if let Some(n) = settings.max_connections {
        config.max_connections = n;
    }
    if let Some(d) = settings.keep_alive {
        config.keep_alive = d;
    }
    if let Some(d) = settings.transaction_timeout {
        config.transaction_timeout = d;
    }
    config.query_timeout = settings.query_timeout;
    let query_memory = settings::query_memory(&settings);
    config.query_max_memory = query_memory;
    if let Some(d) = settings.shutdown_timeout {
        config.shutdown_grace = d;
    }
    let slow_query = settings.slow_query;
    let persistent = settings.storage == Storage::Persistent;
    let sync = if settings.fsync { SyncPolicy::EveryCommit } else { SyncPolicy::OsBuffered };
    let import_dir = settings.import_dir.clone();
    let load_csv_urls = settings.load_csv_urls.clone();
    let gpu = settings.gpu;
    let demo = settings.demo.clone();
    let (tls_cert, tls_key, tls_plain) = (settings.tls_cert.clone(), settings.tls_key.clone(), settings.tls_allow_plain);

    if let Some(name) = &demo
        && let Err(e) = graphbuff::demo_script(name)
    {
        fail(&e.to_string());
    }

    let data = data_dir(&settings);
    let level = settings.log_level;
    let log_dir = match &settings.log_dir {
        LogDir::StandardError => None,
        LogDir::Dir(d) => Some(d.clone()),
        LogDir::Default => data.as_ref().map(|d| d.join("logs")),
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
        _ => None, // the config file checks that both or neither are set
    };
    let admin = first_admin().unwrap_or_else(|e| fatal(&e));

    let mgr = match &data {
        Some(dir) => DatabaseManager::open_with(dir, default_storage(&settings)),
        None => Ok(DatabaseManager::in_memory()),
    };
    let mgr = match mgr {
        Ok(m) => Arc::new(m),
        Err(e) => fatal(&format!(
            "cannot open the data directory {}: {e}",
            data.as_deref().unwrap_or_else(|| std::path::Path::new("")).display()
        )),
    };

    {
        let Some(system) = mgr.system_database() else {
            fatal("this build has no system database for user accounts");
        };
        let users = Users::system(system).unwrap_or_else(|e| fatal(&format!("cannot read the user accounts: {e}")));
        if users.has_admin() {
            if admin.is_some() {
                log.info("there is an administrator already; GRAPHBUFF_ADMIN_USER and GRAPHBUFF_ADMIN_PASSWORD are ignored");
            }
        } else if let Some((name, password)) = &admin {
            if let Err(e) = users.create(name, password, &[Role::Admin], ExistingDatabase::Fail) {
                let hint = if users.roles(name).is_some() {
                    format!("; give it the admin role with `graphbuff-server users grant {name} admin` while the server is stopped")
                } else {
                    String::new()
                };
                fatal(&format!("cannot create the administrator `{name}`: {e}{hint}"));
            }
            log.info(format!("created the administrator `{name}` (GRAPHBUFF_ADMIN_PASSWORD)"));
        } else if config.token.is_some() {
            warn("there is no administrator yet; the token holder can create one (CREATE USER, GRANT ROLE admin TO ...)");
        } else if data.is_none() {
            fatal(
                "there is no administrator, and in-memory accounts are lost at every stop: set \
                 GRAPHBUFF_ADMIN_PASSWORD (or GRAPHBUFF_ADMIN_PASSWORD_FILE), or set security.token",
            );
        } else if prompt::interactive() {
            if let Err(e) = ask_for_admin(&users, None) {
                fatal(&format!("no administrator was created: {e}"));
            }
        } else {
            fatal(
                "there is no administrator yet and no terminal to ask for one: set GRAPHBUFF_ADMIN_PASSWORD (or \
                 GRAPHBUFF_ADMIN_PASSWORD_FILE), create one with `graphbuff-server admin init` while the \
                 server is stopped, start the server in a terminal to be asked, or set security.token",
            );
        }
        config.users = Some(Arc::new(users));
    }

    mgr.set_import_dir(import_dir);
    if !load_csv_urls.is_empty() {
        mgr.set_csv_urls(Some(graphbuff_server::fetch::csv_urls(load_csv_urls)));
    }
    let demo = demo.map(|name| match graphbuff::load_demo(&mgr, &name) {
        Ok(d) => (name, d),
        Err(e) => fatal(&format!("cannot load the demo `{name}`: {e}")),
    });
    if gpu {
        match graphbuff::gpu_kernels() {
            Ok(k) => mgr.set_kernels(k),
            Err(e) => fatal(&format!("queries.gpu: {e}")),
        }
    }
    if let Err(e) = start_embedding(&mgr, &settings.embedding, config_file.as_deref(), &log, &warn) {
        fatal(&format!("embedding: {e}"));
    }
    let server = match Server::start(Arc::clone(&mgr), config.clone()) {
        Ok(s) => s,
        Err(e) => fatal(&format!(
            "cannot listen on {}{}: {e}",
            config.bind,
            config.bolt.iter().chain(config.mcp.as_ref().map(|m| &m.bind)).map(|b| format!(" or {b}")).collect::<String>()
        )),
    };
    let addr = server.local_addr();
    let tls = config.tls.is_some();
    let remote = |a: Option<std::net::SocketAddr>| a.is_some_and(|a| !a.ip().is_loopback());
    if !tls && (!addr.ip().is_loopback() || remote(server.bolt_addr()) || remote(server.mcp_addr())) {
        warn("without TLS (security.tls_cert, tls_key), passwords and tokens cross the network in clear text");
    }
    let (http, bolt) = if tls { ("https", "bolt+s") } else { ("http", "bolt") };
    let listening = format!(
        "graphbuff-server {} listening on {http}://{addr}{}{} ({})",
        env!("CARGO_PKG_VERSION"),
        server.bolt_addr().map_or(String::new(), |b| format!(" and {bolt}://{b}")),
        server.mcp_addr().map_or(String::new(), |m| format!(", MCP on {http}://{m}/mcp")),
        match &data {
            Some(d) => format!("data in {}", d.display()),
            None => "in-memory databases, lost when the server stops".into(),
        }
    );
    println!("{listening}");
    log.info(&listening);
    let fsync = if sync == SyncPolicy::EveryCommit { "fsync every commit" } else { "no fsync" };
    log.info(format_args!(
        "config: {}; new databases: {}; log: {}; slow-query log: {}; logins: {}; query memory limit: {}",
        config_file.as_ref().map_or("none".into(), |f| f.display().to_string()),
        match (&data, persistent) {
            (None, _) => "in memory".to_string(),
            (Some(_), true) => format!("persistent, {fsync}"),
            (Some(_), false) => format!("hybrid, {fsync}"),
        },
        log.dir().map_or("standard error".into(), |d| d.display().to_string()),
        log.slow_query().map_or("off".into(), |d| format!("{} ms", d.as_millis())),
        if config.token.is_some() { "user accounts and token" } else { "user accounts" },
        query_memory.map_or("off".into(), |b| format!("{} MiB", b >> 20))
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
