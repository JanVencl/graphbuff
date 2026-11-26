//! The settings of `graphbuff-server`, from its config file, `graphbuff.toml`.
//!
//! The file is TOML: sections (`[server]`, `[storage]`, …) of `key = value` lines, with
//! strings in quotes, numbers and `true` / `false`; `#` starts a comment. Unknown sections
//! and keys are errors, so a misspelt setting is not silently ignored. Relative paths are
//! relative to the file's folder. [`TEMPLATE`] lists every setting.
//!
//! Where the file is: `--config FILE`, else env `GRAPHBUFF_CONFIG`, else `config/graphbuff.toml`
//! next to the program, else the per-user (or system) one ([`user_config_file`]). With none
//! of them, the server creates one from the template next to the program (or, if that
//! folder cannot be written, in the per-user place).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use graphbuff_server::Level;

/// The config file's name.
pub const FILE_NAME: &str = "graphbuff.toml";

/// The settings, with the defaults for those the file leaves out.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    // [server]
    pub bind: Option<String>,
    pub bolt: Option<String>,
    pub max_connections: Option<usize>,
    pub keep_alive: Option<Duration>,
    pub shutdown_timeout: Option<Duration>,
    /// `server.cors_origins`, checked.
    pub cors_origins: graphbuff_server::Cors,
    // [storage]
    pub storage: Storage,
    /// The data directory; `None`: the per-user default ([`default_data_dir`]).
    pub data: Option<PathBuf>,
    pub fsync: bool,
    pub import_dir: Option<PathBuf>,
    /// `storage.load_csv_urls`, checked: URL prefixes LOAD CSV may read.
    pub load_csv_urls: Vec<String>,
    pub backup_dir: Option<PathBuf>,
    pub demo: Option<String>,
    // [security]
    pub token: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_allow_plain: bool,
    pub max_failed_logins: u32,
    pub login_lockout: Duration,
    // [queries]
    pub transaction_timeout: Option<Duration>,
    pub query_timeout: Option<Duration>,
    /// `queries.max_memory_mb`; `None`: the default ([`default_query_memory`]), `Some(0)`: off.
    pub max_memory_mb: Option<u64>,
    pub gpu: bool,
    pub test_procedures: bool,
    // [logs]
    pub log_dir: LogDir,
    pub log_level: Level,
    pub slow_query: Option<Duration>,
    // [embedding]
    pub embedding: Embedding,
    // [mcp]
    /// `mcp.bind`: where the MCP endpoint listens; `None`: off.
    pub mcp_bind: Option<String>,
    /// `mcp.allowed_origins`, checked.
    pub mcp_origins: graphbuff_server::Cors,
    pub mcp_max_concurrent: usize,
    /// `mcp.calls_per_minute`; 0: no limit.
    pub mcp_calls_per_minute: u32,
    pub mcp_max_rows: usize,
    pub mcp_max_rows_limit: usize,
    pub mcp_max_result_kb: usize,
    pub mcp_query_timeout: Duration,
    /// `mcp.public_url`, checked: turns on OAuth.
    pub mcp_public_url: Option<String>,
    pub mcp_trusted_proxies: Vec<std::net::IpAddr>,
    pub mcp_oauth_clients: Vec<graphbuff_server::OAuthClient>,
    /// `mcp.oauth_clients` as written, for `SHOW SETTINGS`.
    pub mcp_oauth_clients_text: String,
    pub mcp_oauth_client_hosts: Vec<String>,
    /// `mcp.memory_database`: turns on the memory tools.
    pub mcp_memory_database: Option<String>,
}

/// `[embedding]`: the local embedding models (`vector.embed`), in a build with `embed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Embedding {
    /// The model folder; `None`: `model` next to the config file.
    pub model_dir: Option<PathBuf>,
    /// `None`: the only installed model, if there is one.
    pub default_model: Option<String>,
    pub batch_size: usize,
    /// 0: half the cores.
    pub threads: usize,
    pub max_loaded_models: usize,
    pub device: EmbedDevice,
}

impl Default for Embedding {
    fn default() -> Self {
        Embedding { model_dir: None, default_model: None, batch_size: 32, threads: 0, max_loaded_models: 2, device: EmbedDevice::Cpu }
    }
}

/// `embedding.device`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedDevice {
    Cpu,
    Gpu,
}

#[cfg_attr(not(feature = "embed"), allow(dead_code))]
impl Embedding {
    /// The model folder: `model_dir`, else `model` next to `config_file`.
    pub fn model_dir(&self, config_file: Option<&Path>) -> Option<PathBuf> {
        self.model_dir.clone().or_else(|| config_file.map(|f| f.parent().unwrap_or(Path::new("")).join("model")))
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: None,
            bolt: None,
            max_connections: None,
            keep_alive: None,
            shutdown_timeout: None,
            cors_origins: graphbuff_server::Cors::default(),
            storage: Storage::Hybrid,
            data: None,
            fsync: true,
            import_dir: None,
            load_csv_urls: Vec::new(),
            backup_dir: None,
            demo: None,
            token: None,
            tls_cert: None,
            tls_key: None,
            tls_allow_plain: false,
            max_failed_logins: 10,
            login_lockout: Duration::from_secs(60),
            transaction_timeout: None,
            query_timeout: None,
            max_memory_mb: None,
            gpu: false,
            test_procedures: false,
            log_dir: LogDir::Default,
            log_level: Level::Info,
            slow_query: None,
            embedding: Embedding::default(),
            mcp_bind: None,
            mcp_origins: graphbuff_server::Cors::default(),
            mcp_max_concurrent: 8,
            mcp_calls_per_minute: 120,
            mcp_max_rows: 100,
            mcp_max_rows_limit: 1000,
            mcp_max_result_kb: 64,
            mcp_query_timeout: Duration::from_secs(30),
            mcp_public_url: None,
            mcp_trusted_proxies: Vec::new(),
            mcp_oauth_clients: Vec::new(),
            mcp_oauth_clients_text: String::new(),
            mcp_oauth_client_hosts: graphbuff_server::DEFAULT_OAUTH_CLIENT_HOSTS.map(String::from).to_vec(),
            mcp_memory_database: None,
        }
    }
}

/// `storage.mode`: how new databases keep their data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// In memory, made durable by a write-ahead log and snapshots in the data directory.
    Hybrid,
    /// Disk-resident paged storage in the data directory.
    Persistent,
    /// In memory only: lost when the server stops.
    Memory,
}

/// `logs.dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogDir {
    /// The `logs` folder of the data directory; standard error in memory mode.
    Default,
    /// `"-"`: standard error.
    StandardError,
    Dir(PathBuf),
}

/// Written as the config file when the server starts without one, and shipped next to the
/// program. `{data}` is replaced by the `data` line: active next to the program, a comment
/// in the per-user place (where the per-user data directory is the default).
pub const TEMPLATE: &str = r#"# graphbuff-server configuration (TOML).
#
# Change a setting by editing its line (remove the leading `#` to set one that is
# commented out), then restart the server. Relative paths are relative to the folder of
# this file. Env GRAPHBUFF_TOKEN overrides `security.token`; `graphbuff-server --config
# FILE` (or env GRAPHBUFF_CONFIG) reads another file.

[server]
# HTTP address. 127.0.0.1 accepts this machine only; 0.0.0.0 every machine (then also
# set up TLS below).
bind = "127.0.0.1:7475"
# Also serve the Bolt protocol (Neo4j drivers) on this address.
# bolt = "127.0.0.1:7687"
max_connections = 256
# Idle keep-alive timeout of a connection.
keep_alive_seconds = 30
# On stop (SIGTERM, Ctrl-C): how long running queries may take before they are cancelled.
shutdown_timeout_seconds = 5
# Web pages from these origins may call the HTTP API from the browser (CORS): a
# comma-separated list such as "https://app.example.com, http://localhost:5173", or "*".
# cors_origins = "https://app.example.com"

[storage]
# hybrid: in memory, made durable by a log and snapshots in the data folder (default)
# persistent: disk-resident paged storage in the data folder, for graphs larger than memory
# memory: in memory only, lost when the server stops
mode = "hybrid"
# The data folder: databases, user accounts and (by default) logs.
{data}
# Sync every commit to disk. false is faster, but the last commits may be lost when the
# machine (not only the server) crashes.
fsync = true
# Folder LOAD CSV may read files from (off when not set).
# import_dir = "../import"
# Web URLs LOAD CSV may read (http:// or https://), a comma-separated list of prefixes
# that each end in a path, at least "/" (off when not set).
# load_csv_urls = "https://data.example.com/exports/"
# Folder admins may back databases up to and restore them from (off when not set).
# backup_dir = "../backups"
# Load a demo graph into the default database when it is empty: movies.
# demo = "movies"

[security]
# Users log in with their accounts (see `graphbuff-server --help`). A token is also
# accepted as `Authorization: Bearer TOKEN` or as the password of any user name, and its
# holder may do anything.
# token = "change-me"
# HTTPS and Bolt over TLS: a PEM certificate chain and private key.
# tls_cert = "cert.pem"
# tls_key = "key.pem"
# With TLS, also accept clients without TLS on the same ports.
tls_allow_plain = false
# After this many failed logins of one client address and user name (three times as many
# over all names), refuse its logins for login_lockout_seconds, doubling with each further
# failure up to an hour. 0 turns this off.
max_failed_logins = 10
login_lockout_seconds = 60

[queries]
# Roll back a transaction after this long without a request.
transaction_timeout_seconds = 60
# Stop (and roll back) a query that runs longer (no limit when not set; a request may ask
# for less with "timeoutMs").
# query_timeout_ms = 30000
# Stop (and roll back) a query that holds more memory than this (MiB; 0: no limit). Default:
# a quarter of this machine's memory (or of the container's limit), at least 256.
# max_memory_mb = 4096
# Run graph kernels (multi-hop counts, algo.* procedures, vector scores) on the GPU (needs
# a build with the `gpu` feature).
gpu = false

[logs]
# Folder of graphbuff.log and query.log; "-" for standard error. Default: the `logs`
# folder in the data folder (standard error with mode = "memory").
# dir = "../logs"
# error, warn, info or debug (also every connection).
level = "info"
# Log queries that take at least this long to query.log.
# slow_query_ms = 1000

[embedding]
# Text embedding models for vector.embed() (needs a build with the `embed` feature): one
# sub-folder per model, named after it, with model.toml, config.json, tokenizer.json and
# model.safetensors. Default: the `model` folder next to this file.
# model_dir = "model"
# The model used when a query names none. Default: the only installed model.
# default_model = "minilm"
# Texts per forward pass.
batch_size = 32
# Threads that run the models (0: half the cores).
threads = 0
# Models kept in memory at once; the least recently used one is unloaded.
max_loaded_models = 2
# cpu, or gpu (needs a build with the `gpu` feature too).
device = "cpu"

[mcp]
# The Model Context Protocol endpoint for AI agents (Claude, Cursor, ...): set an address to
# turn it on; agents then connect to http://ADDRESS/mcp with an API token (CREATE TOKEN).
# bind = "127.0.0.1:7476"
# Web pages from these origins may call it from the browser: a comma-separated list, or
# "*". Requests from other pages are refused; agents send no origin.
# allowed_origins = "https://app.example.com"
# Tool calls one user may have running at once.
max_concurrent_per_user = 8
# Tool calls per minute and API token (0: no limit).
calls_per_minute = 120
# Rows a query tool returns unless the agent asks for fewer or more, and the most it may
# ask for.
max_rows = 100
max_rows_limit = 1000
# The most KiB of rows a query tool returns.
max_result_kb = 64
# Stop a query of an agent that runs longer (and never longer than queries.query_timeout_ms).
query_timeout_ms = 30000
# OAuth, for hosted agents (claude.ai, ChatGPT) that cannot send a fixed token: the
# endpoint's URL as they reach it, https://HOST/mcp (behind a proxy that terminates TLS
# and forwards to bind). Users then log in on a page of this server and allow the agent.
# public_url = "https://mcp.example.com/mcp"
# Proxies whose X-Forwarded-For header names the client (comma-separated addresses).
# trusted_proxies = "127.0.0.1"
# OAuth clients registered here: "client_id redirect_uri [redirect_uri...]", clients
# separated by ";". Clients with a client metadata document (an https:// client_id, as
# claude.ai and ChatGPT use) need no entry.
# oauth_clients = "my-agent https://agent.example.com/callback"
# Only fetch client metadata documents from these hosts (comma-separated; "*": any
# public host; "": none, only oauth_clients). The default covers claude.ai, Claude
# Desktop, Claude Code, ChatGPT and Codex.
# oauth_client_hosts = "claude.ai, chatgpt.com"
# A memory that agents share: the tools remember, recall and forget keep entities, facts
# about them and relations in this database (created on first use). Everyone who may read
# it sees all memories and may add to them, read-only tokens too.
# memory_database = "memory"
"#;

/// The `data` line of [`TEMPLATE`] for a config file next to the program.
const DATA_NEXT_TO_PROGRAM: &str = r#"data = "../data""#;

/// The `data` line of [`TEMPLATE`] for the per-user config file.
const DATA_PER_USER: &str = "# Default: see `graphbuff-server --help`.\n# data = \"/path/to/data\"";

/// The template for a config file next to the program (`next_to_program`) or in the
/// per-user place.
pub fn template(next_to_program: bool) -> String {
    TEMPLATE.replace("{data}", if next_to_program { DATA_NEXT_TO_PROGRAM } else { DATA_PER_USER })
}

/// A value in the file.
#[derive(Clone, Debug, PartialEq)]
enum Toml {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Toml {
    fn kind(&self) -> &'static str {
        match self {
            Toml::Str(_) => "a string",
            Toml::Int(_) => "a number",
            Toml::Bool(_) => "true or false",
        }
    }
}

/// Every setting, by section: what it holds, and what it does (for `SHOW SETTINGS`).
const SETTINGS: &[(&str, &[(&str, Kind, &str)])] = &[
    (
        "server",
        &[
            ("bind", Kind::Str, "HTTP address."),
            ("bolt", Kind::Str, "Bolt address (off when not set)."),
            ("max_connections", Kind::Positive, "Connections served at once."),
            ("keep_alive_seconds", Kind::Count, "Idle keep-alive timeout of a connection."),
            ("shutdown_timeout_seconds", Kind::Count, "On stop, how long running queries may take before they are cancelled."),
            ("cors_origins", Kind::Str, "Web page origins whose browser apps may call the HTTP API (CORS)."),
        ],
    ),
    (
        "storage",
        &[
            ("mode", Kind::Str, "How new databases keep their data: hybrid, persistent or memory."),
            ("data", Kind::Path, "The data folder: databases, user accounts and (by default) logs."),
            ("fsync", Kind::Bool, "Sync every commit to disk."),
            ("import_dir", Kind::Path, "Folder LOAD CSV may read files from (off when not set)."),
            ("load_csv_urls", Kind::Str, "Web URL prefixes LOAD CSV may read (off when not set)."),
            ("backup_dir", Kind::Path, "Folder of backups and restores (off when not set)."),
            ("demo", Kind::Str, "Demo graph loaded into an empty default database."),
        ],
    ),
    (
        "security",
        &[
            ("token", Kind::Str, "Token accepted as a bearer token or any user's password (value hidden)."),
            ("tls_cert", Kind::Path, "PEM certificate chain for HTTPS and Bolt over TLS."),
            ("tls_key", Kind::Path, "PEM private key for HTTPS and Bolt over TLS."),
            ("tls_allow_plain", Kind::Bool, "With TLS, also accept clients without TLS."),
            ("max_failed_logins", Kind::Count, "Failed logins of one client address and user name before a lockout (0: off)."),
            ("login_lockout_seconds", Kind::Positive, "First lockout after too many failed logins; doubles up to an hour."),
        ],
    ),
    (
        "queries",
        &[
            ("transaction_timeout_seconds", Kind::Positive, "Roll back a transaction after this long without a request."),
            ("query_timeout_ms", Kind::Positive, "Stop a query that runs longer (no limit when not set)."),
            ("max_memory_mb", Kind::Count, "Stop a query that holds more memory (MiB; 0: no limit)."),
            ("gpu", Kind::Bool, "Run graph kernels on the GPU."),
            ("test_procedures", Kind::Bool, "Test procedures for the openCypher TCK."),
        ],
    ),
    (
        "logs",
        &[
            ("dir", Kind::Str, "Folder of the logs; \"-\" for standard error."),
            ("level", Kind::Str, "error, warn, info or debug."),
            ("slow_query_ms", Kind::Count, "Log queries that take at least this long to query.log."),
        ],
    ),
    (
        "embedding",
        &[
            ("model_dir", Kind::Path, "Folder of the embedding models, one sub-folder per model."),
            ("default_model", Kind::Str, "The embedding model used when a query names none."),
            ("batch_size", Kind::Positive, "Texts per forward pass."),
            ("threads", Kind::Count, "Threads that run the models (0: half the cores)."),
            ("max_loaded_models", Kind::Positive, "Embedding models kept in memory at once."),
            ("device", Kind::Str, "Where the models run: cpu or gpu."),
        ],
    ),
    (
        "mcp",
        &[
            ("bind", Kind::Str, "Address of the MCP endpoint for AI agents (off when not set)."),
            ("allowed_origins", Kind::Str, "Web page origins whose browser apps may call the MCP endpoint."),
            ("max_concurrent_per_user", Kind::Positive, "MCP tool calls one user may have running at once."),
            ("calls_per_minute", Kind::Count, "MCP tool calls per minute and API token (0: no limit)."),
            ("max_rows", Kind::Positive, "Rows an MCP query tool returns unless the agent asks otherwise."),
            ("max_rows_limit", Kind::Positive, "The most rows an agent may ask an MCP query tool for."),
            ("max_result_kb", Kind::Positive, "The most KiB of rows an MCP query tool returns."),
            ("query_timeout_ms", Kind::Positive, "Stop a query of an MCP agent that runs longer."),
            ("public_url", Kind::Str, "The MCP endpoint's https:// URL as clients reach it; turns on OAuth."),
            ("trusted_proxies", Kind::Str, "Proxies whose X-Forwarded-For header names the MCP client."),
            ("oauth_clients", Kind::Str, "OAuth clients registered here: client_id and redirect URIs, ';' between clients."),
            ("oauth_client_hosts", Kind::Str, "Hosts whose OAuth client metadata documents are fetched ('*': any, '': none)."),
            ("memory_database", Kind::Str, "The database of the agents' shared memory (no memory tools when not set)."),
        ],
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Str,
    /// A string naming a file or folder, relative to the config file's folder.
    Path,
    Bool,
    /// A number ≥ 0.
    Count,
    /// A number > 0.
    Positive,
}

impl Config {
    /// The settings of the config file `path`, whose text is `text`.
    pub fn parse(path: &Path, text: &str) -> Result<Config, String> {
        let values = read_toml(path, text)?;
        let dir = path.parent().unwrap_or(Path::new(""));
        let mut c = Config::default();
        let str_of = |v: &Toml| match v {
            Toml::Str(s) => s.clone(),
            _ => unreachable!("checked by read_toml"),
        };
        let path_of = |v: &Toml| {
            let p = PathBuf::from(str_of(v));
            if p.is_relative() { dir.join(p) } else { p }
        };
        let int_of = |v: &Toml| match v {
            Toml::Int(i) => *i as u64,
            _ => unreachable!("checked by read_toml"),
        };
        let bool_of = |v: &Toml| matches!(v, Toml::Bool(true));
        for ((section, key), (value, line)) in &values {
            let at = || format!("{}:{line}", path.display());
            match (section.as_str(), key.as_str()) {
                ("server", "bind") => c.bind = Some(str_of(value)),
                ("server", "bolt") => c.bolt = Some(str_of(value)),
                ("server", "max_connections") => c.max_connections = Some(int_of(value) as usize),
                ("server", "keep_alive_seconds") => c.keep_alive = Some(Duration::from_secs(int_of(value))),
                ("server", "shutdown_timeout_seconds") => c.shutdown_timeout = Some(Duration::from_secs(int_of(value))),
                ("server", "cors_origins") => {
                    c.cors_origins = graphbuff_server::Cors::parse(&str_of(value))
                        .map_err(|e| format!("{}: server.cors_origins: {e}", at()))?;
                }
                ("storage", "mode") => {
                    c.storage = match str_of(value).as_str() {
                        "hybrid" => Storage::Hybrid,
                        "persistent" => Storage::Persistent,
                        "memory" => Storage::Memory,
                        other => {
                            return Err(format!(
                                "{}: storage.mode is \"hybrid\", \"persistent\" or \"memory\", not \"{other}\"",
                                at()
                            ));
                        }
                    }
                }
                ("storage", "data") => c.data = Some(path_of(value)),
                ("storage", "fsync") => c.fsync = bool_of(value),
                ("storage", "import_dir") => c.import_dir = Some(path_of(value)),
                ("storage", "load_csv_urls") => {
                    c.load_csv_urls = graphbuff_server::fetch::parse_prefixes(&str_of(value))
                        .map_err(|e| format!("{}: storage.load_csv_urls: {e}", at()))?;
                }
                ("storage", "backup_dir") => c.backup_dir = Some(path_of(value)),
                ("storage", "demo") => c.demo = Some(str_of(value)),
                ("security", "token") => c.token = Some(str_of(value)).filter(|t| !t.is_empty()),
                ("security", "tls_cert") => c.tls_cert = Some(path_of(value)),
                ("security", "tls_key") => c.tls_key = Some(path_of(value)),
                ("security", "tls_allow_plain") => c.tls_allow_plain = bool_of(value),
                ("security", "max_failed_logins") => c.max_failed_logins = int_of(value).min(u64::from(u32::MAX)) as u32,
                ("security", "login_lockout_seconds") => c.login_lockout = Duration::from_secs(int_of(value)),
                ("queries", "transaction_timeout_seconds") => {
                    c.transaction_timeout = Some(Duration::from_secs(int_of(value)));
                }
                ("queries", "query_timeout_ms") => c.query_timeout = Some(Duration::from_millis(int_of(value))),
                ("queries", "max_memory_mb") => c.max_memory_mb = Some(int_of(value)),
                ("queries", "gpu") => c.gpu = bool_of(value),
                ("queries", "test_procedures") => c.test_procedures = bool_of(value),
                ("logs", "dir") => {
                    c.log_dir = match str_of(value).as_str() {
                        "-" => LogDir::StandardError,
                        _ => LogDir::Dir(path_of(value)),
                    }
                }
                ("logs", "level") => {
                    let l = str_of(value);
                    c.log_level = Level::parse(&l).ok_or_else(|| {
                        format!("{}: logs.level is \"error\", \"warn\", \"info\" or \"debug\", not \"{l}\"", at())
                    })?;
                }
                ("logs", "slow_query_ms") => c.slow_query = Some(Duration::from_millis(int_of(value))),
                ("embedding", "model_dir") => c.embedding.model_dir = Some(path_of(value)),
                ("embedding", "default_model") => c.embedding.default_model = Some(str_of(value)).filter(|m| !m.is_empty()),
                ("embedding", "batch_size") => c.embedding.batch_size = int_of(value) as usize,
                ("embedding", "threads") => c.embedding.threads = int_of(value) as usize,
                ("embedding", "max_loaded_models") => c.embedding.max_loaded_models = int_of(value) as usize,
                ("embedding", "device") => {
                    c.embedding.device = match str_of(value).as_str() {
                        "cpu" => EmbedDevice::Cpu,
                        "gpu" => EmbedDevice::Gpu,
                        other => return Err(format!("{}: embedding.device is \"cpu\" or \"gpu\", not \"{other}\"", at())),
                    }
                }
                ("mcp", "bind") => c.mcp_bind = Some(str_of(value)).filter(|b| !b.is_empty()),
                ("mcp", "allowed_origins") => {
                    c.mcp_origins = graphbuff_server::Cors::parse(&str_of(value))
                        .map_err(|e| format!("{}: mcp.allowed_origins: {e}", at()))?;
                }
                ("mcp", "max_concurrent_per_user") => c.mcp_max_concurrent = int_of(value) as usize,
                ("mcp", "calls_per_minute") => c.mcp_calls_per_minute = int_of(value).min(u64::from(u32::MAX)) as u32,
                ("mcp", "max_rows") => c.mcp_max_rows = int_of(value) as usize,
                ("mcp", "max_rows_limit") => c.mcp_max_rows_limit = int_of(value) as usize,
                ("mcp", "max_result_kb") => c.mcp_max_result_kb = int_of(value) as usize,
                ("mcp", "query_timeout_ms") => c.mcp_query_timeout = Duration::from_millis(int_of(value)),
                ("mcp", "public_url") => {
                    let url = str_of(value);
                    if !url.is_empty() {
                        graphbuff_server::check_public_url(&url).map_err(|e| format!("{}: {e}", at()))?;
                        c.mcp_public_url = Some(url);
                    }
                }
                ("mcp", "trusted_proxies") => {
                    c.mcp_trusted_proxies = str_of(value)
                        .split(',')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(|p| p.parse().map_err(|_| format!("{}: mcp.trusted_proxies: `{p}` is not an IP address", at())))
                        .collect::<Result<_, _>>()?;
                }
                ("mcp", "oauth_clients") => {
                    c.mcp_oauth_clients_text = str_of(value);
                    c.mcp_oauth_clients = graphbuff_server::OAuthClient::parse_list(&c.mcp_oauth_clients_text)
                        .map_err(|e| format!("{}: mcp.oauth_clients: {e}", at()))?;
                }
                ("mcp", "memory_database") => {
                    let name = str_of(value);
                    if !name.is_empty() {
                        if graphbuff::cypher::parse_statement(&format!("CREATE DATABASE `{}`", name.replace('`', "``"))).is_err()
                            || name.eq_ignore_ascii_case("system")
                        {
                            return Err(format!("{}: mcp.memory_database: `{name}` is not a database name", at()));
                        }
                        c.mcp_memory_database = Some(name.to_ascii_lowercase());
                    }
                }
                ("mcp", "oauth_client_hosts") => {
                    c.mcp_oauth_client_hosts =
                        str_of(value).split(',').map(str::trim).filter(|h| !h.is_empty()).map(String::from).collect();
                }
                _ => unreachable!("checked by read_toml"),
            }
        }
        if c.storage == Storage::Memory && c.data.is_some() {
            return Err(format!(
                "{}: storage.data is set, but storage.mode = \"memory\" keeps no data on disk",
                path.display()
            ));
        }
        if c.mcp_max_rows > c.mcp_max_rows_limit {
            return Err(format!(
                "{}: mcp.max_rows ({}) is more than mcp.max_rows_limit ({})",
                path.display(),
                c.mcp_max_rows,
                c.mcp_max_rows_limit
            ));
        }
        match (&c.tls_cert, &c.tls_key) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(format!("{}: TLS needs both security.tls_cert and security.tls_key", path.display()));
            }
            (None, None) if c.tls_allow_plain => {
                return Err(format!(
                    "{}: security.tls_allow_plain needs security.tls_cert and security.tls_key",
                    path.display()
                ));
            }
            _ => {}
        }
        Ok(c)
    }

    /// The value of `section.key` in use, as `SHOW SETTINGS` shows it (`None`: not set);
    /// `Err` for a setting this function does not know (a test checks there is none).
    fn value_of(&self, section: &str, key: &str) -> Result<Option<String>, ()> {
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let secs = |d: &Option<Duration>| d.map(|d| d.as_secs().to_string());
        let millis = |d: &Option<Duration>| d.map(|d| d.as_millis().to_string());
        Ok(match (section, key) {
            ("server", "bind") => self.bind.clone(),
            ("server", "bolt") => self.bolt.clone(),
            ("server", "max_connections") => self.max_connections.map(|n| n.to_string()),
            ("server", "keep_alive_seconds") => secs(&self.keep_alive),
            ("server", "shutdown_timeout_seconds") => secs(&self.shutdown_timeout),
            ("server", "cors_origins") => self.cors_origins.is_on().then(|| self.cors_origins.origins().join(", ")),
            ("storage", "mode") => Some(
                match self.storage {
                    Storage::Hybrid => "hybrid",
                    Storage::Persistent => "persistent",
                    Storage::Memory => "memory",
                }
                .into(),
            ),
            ("storage", "data") => path(&self.data),
            ("storage", "fsync") => Some(self.fsync.to_string()),
            ("storage", "import_dir") => path(&self.import_dir),
            ("storage", "load_csv_urls") => (!self.load_csv_urls.is_empty()).then(|| self.load_csv_urls.join(", ")),
            ("storage", "backup_dir") => path(&self.backup_dir),
            ("storage", "demo") => self.demo.clone(),
            ("security", "token") => self.token.as_ref().map(|_| "********".into()),
            ("security", "tls_cert") => path(&self.tls_cert),
            ("security", "tls_key") => path(&self.tls_key),
            ("security", "tls_allow_plain") => Some(self.tls_allow_plain.to_string()),
            ("security", "max_failed_logins") => Some(self.max_failed_logins.to_string()),
            ("security", "login_lockout_seconds") => Some(self.login_lockout.as_secs().to_string()),
            ("queries", "transaction_timeout_seconds") => secs(&self.transaction_timeout),
            ("queries", "query_timeout_ms") => millis(&self.query_timeout),
            ("queries", "max_memory_mb") => self.max_memory_mb.map(|n| n.to_string()),
            ("queries", "gpu") => Some(self.gpu.to_string()),
            ("queries", "test_procedures") => Some(self.test_procedures.to_string()),
            ("logs", "dir") => match &self.log_dir {
                LogDir::Default => None,
                LogDir::StandardError => Some("-".into()),
                LogDir::Dir(d) => Some(d.display().to_string()),
            },
            ("logs", "level") => Some(format!("{:?}", self.log_level).to_ascii_lowercase()),
            ("logs", "slow_query_ms") => millis(&self.slow_query),
            ("embedding", "model_dir") => path(&self.embedding.model_dir),
            ("embedding", "default_model") => self.embedding.default_model.clone(),
            ("embedding", "batch_size") => Some(self.embedding.batch_size.to_string()),
            ("embedding", "threads") => Some(self.embedding.threads.to_string()),
            ("embedding", "max_loaded_models") => Some(self.embedding.max_loaded_models.to_string()),
            ("embedding", "device") => Some(
                match self.embedding.device {
                    EmbedDevice::Cpu => "cpu",
                    EmbedDevice::Gpu => "gpu",
                }
                .into(),
            ),
            ("mcp", "bind") => self.mcp_bind.clone(),
            ("mcp", "allowed_origins") => self.mcp_origins.is_on().then(|| self.mcp_origins.origins().join(", ")),
            ("mcp", "max_concurrent_per_user") => Some(self.mcp_max_concurrent.to_string()),
            ("mcp", "calls_per_minute") => Some(self.mcp_calls_per_minute.to_string()),
            ("mcp", "max_rows") => Some(self.mcp_max_rows.to_string()),
            ("mcp", "max_rows_limit") => Some(self.mcp_max_rows_limit.to_string()),
            ("mcp", "max_result_kb") => Some(self.mcp_max_result_kb.to_string()),
            ("mcp", "query_timeout_ms") => Some(self.mcp_query_timeout.as_millis().to_string()),
            ("mcp", "public_url") => self.mcp_public_url.clone(),
            ("mcp", "trusted_proxies") => (!self.mcp_trusted_proxies.is_empty())
                .then(|| self.mcp_trusted_proxies.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")),
            ("mcp", "oauth_clients") => (!self.mcp_oauth_clients_text.is_empty()).then(|| self.mcp_oauth_clients_text.clone()),
            ("mcp", "memory_database") => self.mcp_memory_database.clone(),
            ("mcp", "oauth_client_hosts") => Some(self.mcp_oauth_client_hosts.join(", ")),
            _ => return Err(()),
        })
    }

    /// Every setting with its value and default, for `SHOW SETTINGS`.
    pub fn listing(&self) -> Vec<graphbuff_server::Setting> {
        let defaults = Config::default();
        SETTINGS
            .iter()
            .flat_map(|(section, keys)| keys.iter().map(move |(key, _, description)| (*section, *key, *description)))
            .map(|(section, key, description)| graphbuff_server::Setting {
                name: format!("{section}.{key}"),
                value: self.value_of(section, key).unwrap_or_default(),
                default: defaults.value_of(section, key).unwrap_or_default(),
                description: description.to_string(),
            })
            .collect()
    }

    /// Applies the environment: GRAPHBUFF_TOKEN sets the token (a secret, kept out of the
    /// file in containers).
    pub fn apply_env(&mut self) {
        if let Some(t) = std::env::var("GRAPHBUFF_TOKEN").ok().filter(|t| !t.is_empty()) {
            self.token = Some(t);
        }
    }

    /// The data directory, or `None` in memory mode.
    pub fn data_dir(&self) -> Result<Option<PathBuf>, String> {
        if self.storage == Storage::Memory {
            return Ok(None);
        }
        match &self.data {
            Some(d) => Ok(Some(d.clone())),
            None => default_data_dir().map(Some).ok_or_else(|| {
                "no default data folder (HOME or LOCALAPPDATA is not set): set storage.data in the config file".into()
            }),
        }
    }
}

/// The settings of a config file: `(section, key) → (value, line)`.
type Values = HashMap<(String, String), (Toml, usize)>;

/// Reads the TOML subset the config file uses, checking every section, key and value kind.
fn read_toml(path: &Path, text: &str) -> Result<Values, String> {
    let mut out = HashMap::new();
    let mut section: Option<&'static [(&'static str, Kind, &'static str)]> = None;
    let mut section_name = String::new();
    let mut seen_sections: Vec<String> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let at = || format!("{}:{line_no}", path.display());
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let (name, after) = rest.split_once(']').ok_or_else(|| format!("{}: expected `[section]`", at()))?;
            let after = after.trim();
            if !(after.is_empty() || after.starts_with('#')) {
                return Err(format!("{}: unexpected text after `[{name}]`", at()));
            }
            let name = name.trim();
            let (_, keys) = SETTINGS.iter().find(|(s, _)| *s == name).ok_or_else(|| {
                let known: Vec<String> = SETTINGS.iter().map(|(s, _)| format!("[{s}]")).collect();
                format!("{}: unknown section `[{name}]` (sections: {})", at(), known.join(", "))
            })?;
            if seen_sections.iter().any(|s| s == name) {
                return Err(format!("{}: section `[{name}]` appears twice", at()));
            }
            seen_sections.push(name.to_string());
            section = Some(keys);
            section_name = name.to_string();
            continue;
        }
        let (key, rest) = line.split_once('=').ok_or_else(|| format!("{}: expected `key = value`", at()))?;
        let key = key.trim();
        let Some(keys) = section else {
            return Err(format!("{}: `{key}` is outside a section; settings go under [server], [storage], …", at()));
        };
        let &(_, kind, _) = keys.iter().find(|(k, ..)| *k == key).ok_or_else(|| {
            let known: Vec<&str> = keys.iter().map(|(k, ..)| *k).collect();
            format!("{}: unknown setting `{key}` in [{section_name}] (settings: {})", at(), known.join(", "))
        })?;
        let value = parse_value(rest.trim()).map_err(|e| format!("{}: {key}: {e}", at()))?;
        let fits = match (kind, &value) {
            (Kind::Str | Kind::Path, Toml::Str(_)) | (Kind::Bool, Toml::Bool(_)) => true,
            (Kind::Count, Toml::Int(i)) => *i >= 0,
            (Kind::Positive, Toml::Int(i)) => *i > 0,
            _ => false,
        };
        if !fits {
            let wanted = match kind {
                Kind::Str => "a string in quotes",
                Kind::Path => "a path in quotes",
                Kind::Bool => "true or false",
                Kind::Count => "a number (0 or more)",
                Kind::Positive => "a number greater than 0",
            };
            return Err(format!("{}: {section_name}.{key} needs {wanted}, not {}", at(), match &value {
                Toml::Int(i) => i.to_string(),
                v => v.kind().to_string(),
            }));
        }
        let entry = (section_name.clone(), key.to_string());
        if out.contains_key(&entry) {
            return Err(format!("{}: {section_name}.{key} is set twice", at()));
        }
        out.insert(entry, (value, line_no));
    }
    Ok(out)
}

/// A value and an optional comment after it.
fn parse_value(text: &str) -> Result<Toml, String> {
    let rest_is_comment = |rest: &str| {
        let rest = rest.trim();
        if rest.is_empty() || rest.starts_with('#') {
            Ok(())
        } else {
            Err(format!("unexpected `{rest}` after the value"))
        }
    };
    if let Some(body) = text.strip_prefix('"') {
        let mut s = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    rest_is_comment(&body[i + 1..])?;
                    return Ok(Toml::Str(s));
                }
                '\\' => {
                    let (_, e) = chars.next().ok_or("unfinished escape")?;
                    s.push(match e {
                        '"' => '"',
                        '\\' => '\\',
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        'u' => {
                            let hex: String = (0..4).filter_map(|_| chars.next().map(|(_, c)| c)).collect();
                            u32::from_str_radix(&hex, 16)
                                .ok()
                                .and_then(char::from_u32)
                                .ok_or_else(|| format!("bad escape `\\u{hex}`"))?
                        }
                        other => {
                            return Err(format!(
                                "unknown escape `\\{other}` (on Windows, write paths with `/` or in 'single quotes')"
                            ));
                        }
                    });
                }
                c => s.push(c),
            }
        }
        return Err("the string has no closing `\"`".into());
    }
    if let Some(body) = text.strip_prefix('\'') {
        let (s, rest) = body.split_once('\'').ok_or("the string has no closing `'`")?;
        rest_is_comment(rest)?;
        return Ok(Toml::Str(s.to_string()));
    }
    let (word, rest) = text.split_once('#').map_or((text, ""), |(w, r)| (w, r));
    let _ = rest;
    let word = word.trim();
    match word {
        "true" => return Ok(Toml::Bool(true)),
        "false" => return Ok(Toml::Bool(false)),
        "" => return Err("needs a value".into()),
        _ => {}
    }
    word.replace('_', "")
        .parse::<i64>()
        .map(Toml::Int)
        .map_err(|_| format!("`{word}` is not a value; strings go in quotes (\"…\"), and true / false and numbers without"))
}

/// The config file next to the program: `config/graphbuff.toml` in the program's folder.
pub fn program_config_file() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("config").join(FILE_NAME))
}

/// The per-user (or system) config file:
///   Linux    $XDG_CONFIG_HOME/graphbuff/graphbuff.toml (~/.config/graphbuff/graphbuff.toml),
///            /etc/graphbuff/graphbuff.toml when run as root
///   macOS    ~/Library/Application Support/graphbuff/graphbuff.toml
///   Windows  %LOCALAPPDATA%\graphbuff\graphbuff.toml
pub fn user_config_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join(FILE_NAME))
}

/// The folder of [`user_config_file`].
pub fn config_dir() -> Option<PathBuf> {
    let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("LOCALAPPDATA").map(|d| d.join("graphbuff"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/graphbuff"))
    } else if running_as_root() {
        Some("/etc/graphbuff".into())
    } else {
        env("XDG_CONFIG_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".config")))
            .map(|d| d.join("graphbuff"))
    }
}

/// Where databases go without `storage.data`.
pub fn default_data_dir() -> Option<PathBuf> {
    let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return env("LOCALAPPDATA").map(|d| d.join("graphbuff"));
    }
    if cfg!(target_os = "macos") {
        return env("HOME").map(|h| h.join("Library/Application Support/graphbuff"));
    }
    if running_as_root() {
        return Some("/var/lib/graphbuff".into());
    }
    env("XDG_DATA_HOME")
        .or_else(|| env("HOME").map(|h| h.join(".local/share")))
        .map(|d| d.join("graphbuff"))
}

/// Whether the effective user is root (Linux; elsewhere: no).
fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let uid = s.lines().find(|l| l.starts_with("Uid:"))?.split_whitespace().nth(2)?.to_string();
            Some(uid == "0")
        })
        .unwrap_or(false)
}

/// The memory limit of a query when `queries.max_memory_mb` is not set, in bytes: a
/// quarter of the machine's (or container's) memory, at least 256 MiB; 2 GiB when that is
/// unknown.
pub fn default_query_memory() -> u64 {
    graphbuff::accounting::total_memory().map_or(2 << 30, |total| (total / 4).max(256 << 20))
}

/// The memory limit of a query from `queries.max_memory_mb`, in bytes; `None`: no limit.
pub fn query_memory(c: &Config) -> Option<u64> {
    match c.max_memory_mb {
        None => Some(default_query_memory()),
        Some(0) => None,
        Some(mb) => Some(mb.saturating_mul(1 << 20)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, String> {
        Config::parse(&Path::new("conf").join("graphbuff.toml"), text)
    }

    #[test]
    fn sections_values_and_paths() {
        let c = parse(
            "# comment\n[server]\nbind = \"0.0.0.0:1\" # trailing\nmax_connections = 1_000\n\n\
             [storage]\nmode = 'persistent'\ndata = \"d\"\nfsync = false\nbackup_dir = \"/abs/b\"\n\
             [logs]\ndir = \"-\"\nlevel = \"debug\"\n[queries]\ngpu = true\nquery_timeout_ms = 250\n",
        )
        .unwrap();
        assert_eq!(c.bind.as_deref(), Some("0.0.0.0:1"));
        assert_eq!(c.max_connections, Some(1000));
        assert_eq!(c.storage, Storage::Persistent);
        assert_eq!(c.data, Some(Path::new("conf").join("d")));
        assert!(!c.fsync && c.gpu);
        assert_eq!(c.backup_dir, Some(PathBuf::from("/abs/b")));
        assert_eq!(c.log_dir, LogDir::StandardError);
        assert_eq!(c.log_level, Level::Debug);
        assert_eq!(c.query_timeout, Some(Duration::from_millis(250)));
        assert_eq!(query_memory(&c), Some(default_query_memory()));
        let off = parse("[queries]
max_memory_mb = 0").unwrap();
        assert_eq!(query_memory(&off), None);
        let set = parse("[queries]
max_memory_mb = 512").unwrap();
        assert_eq!(query_memory(&set), Some(512 << 20));
        assert!(default_query_memory() >= 256 << 20);
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(parse("[security]\ntoken = \"a\\\"b\\u00e9\"").unwrap().token.as_deref(), Some("a\"bé"));
    }

    #[test]
    fn embedding_section() {
        let c = parse(
            "[embedding]\nmodel_dir = \"models\"\ndefault_model = \"minilm\"\nbatch_size = 8\nthreads = 3\n\
             max_loaded_models = 1\ndevice = \"gpu\"\n",
        )
        .unwrap();
        let e = &c.embedding;
        assert_eq!(e.model_dir, Some(Path::new("conf").join("models")));
        assert_eq!(e.default_model.as_deref(), Some("minilm"));
        assert_eq!((e.batch_size, e.threads, e.max_loaded_models, e.device), (8, 3, 1, EmbedDevice::Gpu));
        let file = Path::new("conf").join("graphbuff.toml");
        assert_eq!(e.model_dir(Some(&file)), Some(Path::new("conf").join("models")));
        let d = Embedding::default();
        assert_eq!(d.model_dir(Some(&file)), Some(Path::new("conf").join("model")));
        assert_eq!(d.model_dir(None), None);
        assert_eq!(parse("[embedding]\ndefault_model = \"\"").unwrap().embedding.default_model, None);
        let err = |t: &str| parse(t).unwrap_err();
        assert!(err("[embedding]\ndevice = \"tpu\"").contains("embedding.device"));
        assert!(err("[embedding]\nbatch_size = 0").contains("greater than 0"));
        assert!(err("[embedding]\nmax_loaded_models = 0").contains("greater than 0"));
        let list = c.listing();
        let get = |n: &str| list.iter().find(|s| s.name == n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(get("embedding.device").value.as_deref(), Some("gpu"));
        assert_eq!(get("embedding.device").default.as_deref(), Some("cpu"));
        assert_eq!(get("embedding.batch_size").default.as_deref(), Some("32"));
    }

    #[test]
    fn oauth_client_hosts() {
        let hosts = |t: &str| parse(t).unwrap().mcp_oauth_client_hosts;
        assert_eq!(hosts(""), ["claude.ai", "chatgpt.com"], "the hosted clients by default");
        assert_eq!(hosts("[mcp]\noauth_client_hosts = \"a.test, B.test\""), ["a.test", "B.test"]);
        assert_eq!(hosts("[mcp]\noauth_client_hosts = \"*\""), ["*"]);
        assert!(hosts("[mcp]\noauth_client_hosts = \"\"").is_empty(), "none");
        let list = parse("[mcp]\noauth_client_hosts = \"\"").unwrap().listing();
        let row = list.iter().find(|s| s.name == "mcp.oauth_client_hosts").unwrap();
        assert_eq!((row.value.as_deref(), row.default.as_deref()), (Some(""), Some("claude.ai, chatgpt.com")));
    }

    #[test]
    fn errors_name_the_line() {
        let e = |text: &str| parse(text).unwrap_err();
        assert!(e("[server]\nbnd = \"x\"").contains("graphbuff.toml:2: unknown setting `bnd` in [server]"), "{}", e("[server]\nbnd = \"x\""));
        assert!(e("[srv]").contains("unknown section `[srv]`"));
        assert!(e("bind = \"x\"").contains("outside a section"));
        assert!(e("[server]\nbind = 127.0.0.1").contains("not a value"));
        assert!(e("[server]\nmax_connections = 0").contains("greater than 0"));
        assert!(e("[server]\nmax_connections = \"8\"").contains("needs a number greater than 0, not a string"));
        assert!(e("[storage]\nfsync = yes").contains("not a value"));
        assert!(e("[storage]\nmode = \"disk\"").contains("not \"disk\""));
        assert!(e("[storage]\nmode = \"memory\"\ndata = \"d\"").contains("keeps no data on disk"));
        assert!(e("[server]\nbind = \"a\"\nbind = \"b\"").contains("set twice"));
        assert!(e("[server]\n[server]").contains("appears twice"));
        assert!(e("[storage]\ndata = \"C:\\data\"").contains("single quotes"));
        assert!(e("[security]\ntls_cert = \"c.pem\"").contains("needs both"));
        assert!(e("[logs]\nlevel = \"loud\"").contains("logs.level"));
        assert!(e("[server]\nbind = \"x").contains("no closing"));
        assert!(e("[server]\nbind = \"x\" y").contains("after the value"));
        assert!(e("[server]\ncors_origins = \"example.com\"").contains("server.cors_origins"));
        assert!(e("[mcp]\nallowed_origins = \"example.com\"").contains("mcp.allowed_origins"));
        assert!(e("[mcp]\nmax_concurrent_per_user = 0").contains("greater than 0"));
        assert!(e("[mcp]\nmax_rows = 2000").contains("more than mcp.max_rows_limit"));
        assert!(e("[mcp]\npublic_url = \"http://mcp.example.com/mcp\"").contains("https://"));
        assert!(e("[mcp]\ntrusted_proxies = \"proxy\"").contains("not an IP address"));
        assert!(e("[mcp]\noauth_clients = \"app http://example.com/cb\"").contains("mcp.oauth_clients"));
        let on = Config::parse(
            Path::new("x.toml"),
            "[mcp]\nbind = \"127.0.0.1:7476\"\npublic_url = \"https://mcp.example.com/mcp\"\noauth_clients = \"app https://a.example/cb\"",
        )
        .unwrap();
        assert_eq!(on.mcp_public_url.as_deref(), Some("https://mcp.example.com/mcp"));
        assert_eq!(on.mcp_oauth_clients.len(), 1);
        assert!(e("[storage]\nload_csv_urls = \"https://a.example\"").contains("storage.load_csv_urls"));
        let urls = parse("[storage]\nload_csv_urls = \"https://a.example/x/, http://b.example/\"").unwrap();
        assert_eq!(urls.load_csv_urls, ["https://a.example/x/", "http://b.example/"]);
    }

    #[test]
    fn listing_shows_values_and_hides_the_token() {
        let c = parse("[server]\nbind = \"0.0.0.0:1\"\n[security]\ntoken = \"s3cret\"\n[logs]\nlevel = \"warn\"").unwrap();
        let list = c.listing();
        let get = |n: &str| list.iter().find(|s| s.name == n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(get("server.bind").value.as_deref(), Some("0.0.0.0:1"));
        assert_eq!(get("server.bind").default, None);
        assert_eq!(get("security.token").value.as_deref(), Some("********"));
        assert_eq!(get("logs.level").value.as_deref(), Some("warn"));
        assert_eq!(get("logs.level").default.as_deref(), Some("info"));
        assert_eq!(get("storage.mode").value.as_deref(), Some("hybrid"));
        assert_eq!(list.len(), SETTINGS.iter().map(|(_, k)| k.len()).sum::<usize>());
        assert!(!format!("{list:?}").contains("s3cret"));
        for (section, keys) in SETTINGS {
            for (key, ..) in keys.iter() {
                assert!(c.value_of(section, key).is_ok(), "SHOW SETTINGS has no value for {section}.{key}");
            }
        }
    }

    #[test]
    fn templates_parse() {
        let next = Config::parse(Path::new("cfg/graphbuff.toml"), &template(true)).unwrap();
        assert_eq!(next.data, Some(Path::new("cfg").join("../data")));
        assert_eq!(next.bind.as_deref(), Some("127.0.0.1:7475"));
        let user = Config::parse(Path::new("u/graphbuff.toml"), &template(false)).unwrap();
        assert_eq!(user.data, None);
        assert_eq!((user.mcp_bind, user.mcp_max_concurrent, user.mcp_calls_per_minute), (None, 8, 120));
        let on = Config::parse(Path::new("x.toml"), "[mcp]\nbind = \"127.0.0.1:7476\"\ncalls_per_minute = 0").unwrap();
        assert_eq!((on.mcp_bind.as_deref(), on.mcp_calls_per_minute), (Some("127.0.0.1:7476"), 0));
    }

    /// The files the release archives and the Docker image ship.
    #[test]
    fn shipped_files() {
        let shipped = include_str!("../../../packaging/config/graphbuff.toml");
        assert_eq!(shipped.replace("\r", ""), template(true), "packaging/config/graphbuff.toml is the template");
        let docker = Config::parse(
            Path::new("/etc/graphbuff/graphbuff.toml"),
            include_str!("../../../packaging/docker/graphbuff.toml"),
        )
        .unwrap();
        assert_eq!(docker.bind.as_deref(), Some("0.0.0.0:7475"));
        assert_eq!(docker.bolt.as_deref(), Some("0.0.0.0:7687"));
        assert_eq!(docker.data, Some(PathBuf::from("/data")));
        assert_eq!(docker.log_dir, LogDir::StandardError);
        assert_eq!(docker.embedding.model_dir, Some(PathBuf::from("/models")));
    }
}
