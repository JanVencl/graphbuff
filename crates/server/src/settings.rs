//! The settings of `graphbuff-server`, from (lowest to highest precedence) the config
//! file, the environment and the command line.
//!
//! The config file holds one `key = value` per line, where the keys are the long options
//! without `--` (`bind = 0.0.0.0:7475`, `in-memory = true`); `#` starts a comment.
//! Relative paths in it are relative to the file's directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Every setting: its name (the long option without `--`), and whether it is a flag
/// (`--gpu` on the command line, `gpu = true` in the file) rather than taking a value.
const KEYS: &[(&str, bool)] = &[
    ("bind", false),
    ("bolt", false),
    ("data", false),
    ("in-memory", true),
    ("demo", false),
    ("persistent", true),
    ("no-fsync", true),
    ("token", false),
    ("tls-cert", false),
    ("tls-key", false),
    ("tls-allow-plain", true),
    ("max-connections", false),
    ("keep-alive", false),
    ("tx-timeout", false),
    ("query-timeout", false),
    ("shutdown-timeout", false),
    ("import-dir", false),
    ("backup-dir", false),
    ("gpu", true),
    ("log-dir", false),
    ("log-level", false),
    ("slow-query", false),
    ("test-procedures", true),
];

/// Settings that name a file or directory: relative ones in the config file are resolved
/// against the file's directory.
const PATHS: &[&str] = &["data", "tls-cert", "tls-key", "import-dir", "backup-dir", "log-dir"];

/// Environment variables and the settings they stand for.
const ENV: &[(&str, &str)] = &[
    ("GRAPHBUFF_TOKEN", "token"),
    ("GRAPHBUFF_DATA", "data"),
    ("GRAPHBUFF_LOG_DIR", "log-dir"),
    ("GRAPHBUFF_LOG_LEVEL", "log-level"),
];

/// Written to the default config location the first time the server starts without one.
pub const TEMPLATE: &str = "\
# graphbuff-server configuration.
#
# One `key = value` per line; the keys are the command-line options without `--`.
# Command-line options and environment variables (GRAPHBUFF_DATA, GRAPHBUFF_LOG_DIR,
# GRAPHBUFF_LOG_LEVEL, GRAPHBUFF_TOKEN) override this file. Relative
# paths are relative to this file's directory. `graphbuff-server --help` describes
# every setting.

# Network
# bind = 127.0.0.1:7475
# bolt = 127.0.0.1:7687
# max-connections = 256
# keep-alive = 30

# Storage (default data directory: see --help)
# data = /path/to/data
# in-memory = false
# persistent = false
# no-fsync = false
# import-dir = import
# backup-dir = backups

# Logs (default: the `logs` folder in the data directory)
# log-dir = logs
# log-level = info
# slow-query = 1000

# Security
# token = change-me
# tls-cert = cert.pem
# tls-key = key.pem
# tls-allow-plain = false

# Queries and shutdown
# tx-timeout = 60
# query-timeout = 30000
# shutdown-timeout = 5
# gpu = false
";

/// Where a setting came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    File,
    Env,
    Command,
}

#[derive(Default)]
pub struct Settings {
    values: HashMap<&'static str, (String, Origin)>,
}

impl Settings {
    /// Reads the command line (without the program name); returns the config file it
    /// names (`--config`) and the remaining settings to apply, in order.
    pub fn command_line(args: impl Iterator<Item = String>) -> Result<CommandLine, String> {
        let mut out = CommandLine::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            if arg == "-h" || arg == "--help" {
                out.help = true;
                continue;
            }
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unknown option `{arg}`"));
            };
            if name == "config" {
                out.config = Some(args.next().ok_or("--config needs a value")?);
                continue;
            }
            let &(key, flag) = KEYS
                .iter()
                .find(|(k, _)| *k == name)
                .ok_or_else(|| format!("unknown option `{arg}`"))?;
            let value = if flag {
                "true".to_string()
            } else {
                args.next().ok_or_else(|| format!("--{key} needs a value"))?
            };
            out.settings.push((key, value));
        }
        Ok(out)
    }

    /// Applies the config file at `path`.
    pub fn apply_file(&mut self, path: &Path, text: &str) -> Result<(), String> {
        let dir = path.parent().unwrap_or(Path::new(""));
        let mut seen: Vec<&str> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let at = || format!("{}:{}", path.display(), n + 1);
            let line = line.split_once('#').map_or(line, |(l, _)| l).trim();
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once('=').ok_or_else(|| format!("{}: expected `key = value`", at()))?;
            let (name, value) = (name.trim(), value.trim());
            let &(key, flag) = KEYS
                .iter()
                .find(|(k, _)| *k == name)
                .ok_or_else(|| format!("{}: unknown setting `{name}`", at()))?;
            if seen.contains(&key) {
                return Err(format!("{}: `{key}` is set twice", at()));
            }
            seen.push(key);
            let value = if flag {
                match value {
                    "true" | "yes" | "on" => "true".to_string(),
                    "false" | "no" | "off" => "false".to_string(),
                    _ => return Err(format!("{}: `{key}` needs true or false", at())),
                }
            } else if value.is_empty() {
                return Err(format!("{}: `{key}` needs a value", at()));
            } else if PATHS.contains(&key) && value != "-" && Path::new(value).is_relative() {
                dir.join(value).display().to_string()
            } else {
                value.to_string()
            };
            self.set(key, value, Origin::File)?;
        }
        Ok(())
    }

    /// Applies the environment variables that stand for settings.
    pub fn apply_env(&mut self) -> Result<(), String> {
        for &(var, key) in ENV {
            if let Some(v) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
                self.set(key, v, Origin::Env)?;
            }
        }
        Ok(())
    }

    /// Applies the settings of the command line.
    pub fn apply_command_line(&mut self, settings: Vec<(&'static str, String)>) -> Result<(), String> {
        for (key, value) in settings {
            self.set(key, value, Origin::Command)?;
        }
        Ok(())
    }

    fn set(&mut self, key: &'static str, value: String, origin: Origin) -> Result<(), String> {
        // `data` and `in-memory` replace each other, except from the same place.
        let other = match key {
            "data" => Some("in-memory"),
            "in-memory" if value == "true" => Some("data"),
            _ => None,
        };
        if let Some(other) = other {
            match self.values.get(other) {
                Some((v, o)) if *o == origin && (other == "data" || v == "true") => {
                    return Err("--in-memory and --data exclude each other".into());
                }
                _ => {
                    self.values.remove(other);
                }
            }
        }
        self.values.insert(key, (value, origin));
        Ok(())
    }

    /// The value of a setting.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|(v, _)| v.as_str())
    }

    /// Whether a flag is on.
    pub fn flag(&self, key: &str) -> bool {
        self.get(key) == Some("true")
    }
}

#[derive(Default)]
pub struct CommandLine {
    pub help: bool,
    pub config: Option<String>,
    pub settings: Vec<(&'static str, String)>,
}

/// Where the config file is read from without --config or GRAPHBUFF_CONFIG:
///   Linux    $XDG_CONFIG_HOME/graphbuff/graphbuff.conf (~/.config/graphbuff/graphbuff.conf),
///            /etc/graphbuff/graphbuff.conf when run as root
///   macOS    ~/Library/Application Support/graphbuff/graphbuff.conf
///   Windows  %LOCALAPPDATA%\graphbuff\graphbuff.conf
pub fn default_config_file() -> Option<PathBuf> {
    let env = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(windows) {
        env("LOCALAPPDATA").map(|d| d.join("graphbuff"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/graphbuff"))
    } else if running_as_root() {
        Some("/etc/graphbuff".into())
    } else {
        env("XDG_CONFIG_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".config")))
            .map(|d| d.join("graphbuff"))
    };
    dir.map(|d| d.join("graphbuff.conf"))
}

/// Where databases go without --data, GRAPHBUFF_DATA or `data` in the config file.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_paths() {
        let mut s = Settings::default();
        let file = Path::new("conf").join("graphbuff.conf");
        s.apply_file(&file, "bind = 0.0.0.0:1 # comment\ndata = d\ngpu = yes\nlog-dir = -\n").unwrap();
        assert_eq!(s.get("data"), Some(Path::new("conf").join("d").display().to_string().as_str()));
        assert!(s.flag("gpu"));
        assert_eq!(s.get("log-dir"), Some("-"));
        let cl = Settings::command_line(["--bind", "127.0.0.1:2", "--in-memory"].map(String::from).into_iter()).unwrap();
        s.apply_command_line(cl.settings).unwrap();
        assert_eq!(s.get("bind"), Some("127.0.0.1:2"));
        assert!(s.flag("in-memory"));
        assert_eq!(s.get("data"), None);
    }

    #[test]
    fn errors() {
        let mut s = Settings::default();
        let f = Path::new("x.conf");
        assert!(s.apply_file(f, "nope = 1").unwrap_err().contains("unknown setting `nope`"));
        assert!(s.apply_file(f, "gpu = maybe").unwrap_err().contains("true or false"));
        assert!(s.apply_file(f, "bind").unwrap_err().contains("key = value"));
        assert!(Settings::command_line(["--nope"].map(String::from).into_iter()).is_err());
        let mut s = Settings::default();
        let cl = Settings::command_line(["--in-memory", "--data", "x"].map(String::from).into_iter()).unwrap();
        assert!(s.apply_command_line(cl.settings).unwrap_err().contains("exclude each other"));
    }

    #[test]
    fn template_parses() {
        Settings::default().apply_file(Path::new("t.conf"), TEMPLATE).unwrap();
    }
}
