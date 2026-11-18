//! The real binary: user accounts in the system database, the first administrator from
//! GRAPHBUFF_ADMIN_USER and GRAPHBUFF_ADMIN_PASSWORD (or _FILE) when there are no
//! accounts yet, and the offline `graphbuff-server users` command.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_graphbuff-server");

/// Every variable the server reads, so the caller's environment does not leak in.
const VARS: &[&str] = &[
    "GRAPHBUFF_TOKEN",
    "GRAPHBUFF_DATA",
    "GRAPHBUFF_LOG_DIR",
    "GRAPHBUFF_LOG_LEVEL",
    "GRAPHBUFF_ADMIN_USER",
    "GRAPHBUFF_ADMIN_PASSWORD",
    "GRAPHBUFF_ADMIN_PASSWORD_FILE",
];

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphbuff-bootstrap-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The server with `args` (in memory unless they name --data), in a clean environment.
fn command(args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0", "--log-dir", "-"]);
    if !args.contains(&"--data") {
        cmd.arg("--in-memory");
    }
    cmd.args(args);
    clean(&mut cmd, env);
    cmd
}

/// `graphbuff-server users ARGS…` in a clean environment; returns standard output, or
/// standard error on failure.
fn users(args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new(BIN);
    cmd.arg("users").args(args).stdin(Stdio::null());
    clean(&mut cmd, env);
    let out = cmd.output().unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn clean(cmd: &mut Command, env: &[(&str, &str)]) {
    cmd.env("GRAPHBUFF_CONFIG", "-");
    for key in VARS {
        cmd.env_remove(key);
    }
    cmd.env_remove("GRAPHBUFF_PASSWORD");
    for (k, v) in env {
        cmd.env(k, v);
    }
}

/// Starts the server, checks each `(user, password, expected status)` login over HTTP,
/// and stops it.
fn logins(args: &[&str], env: &[(&str, &str)], checks: &[(&str, &str, u16)]) {
    logins_while(args, env, checks, || {});
}

/// Like [`logins`], also running `during` while the server runs.
fn logins_while(args: &[&str], env: &[(&str, &str)], checks: &[(&str, &str, u16)], during: impl FnOnce()) {
    // No terminal on standard input, so the server never waits for an answer.
    let mut child = command(args, env).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut listening = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut listening).unwrap();
    let result = std::panic::catch_unwind(|| {
        let addr = listening
            .split("http://")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("no address in {listening:?}"));
        for &(user, password, status) in checks {
            assert_eq!(query(addr, user, password), status, "{user}:{password}");
        }
    });
    let result = result.and_then(|()| std::panic::catch_unwind(std::panic::AssertUnwindSafe(during)));
    child.kill().unwrap();
    child.wait().unwrap();
    result.unwrap();
}

/// The HTTP status of `RETURN 1` as `user` (no credentials when it is empty).
fn query(addr: &str, user: &str, password: &str) -> u16 {
    let body = r#"{"query": "RETURN 1"}"#;
    let auth = if user.is_empty() {
        String::new()
    } else {
        format!("Authorization: Basic {}\r\n", base64(format!("{user}:{password}").as_bytes()))
    };
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        conn,
        "POST /query HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    reply.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("{reply}"))
}

fn base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Runs the server expecting it to refuse to start; returns its standard error.
fn refused(args: &[&str], env: &[(&str, &str)]) -> String {
    let out = command(args, env).stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_first_start_creates_the_administrator_and_later_starts_keep_it() {
    let dir = temp_dir("first");
    let data = dir.join("data");
    let data = data.to_str().unwrap();
    logins(
        &["--data", data],
        &[("GRAPHBUFF_ADMIN_PASSWORD", "first-pw")],
        &[("admin", "first-pw", 200), ("admin", "wrong", 401), ("", "", 401)],
    );
    // There is an account now: a different password in the environment is ignored.
    logins(
        &["--data", data],
        &[("GRAPHBUFF_ADMIN_PASSWORD", "second-pw")],
        &[("admin", "first-pw", 200), ("admin", "second-pw", 401)],
    );
    assert!(std::path::Path::new(data).join("system").is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn password_file_and_user_name() {
    let dir = temp_dir("file");
    let secret = dir.join("secret");
    std::fs::write(&secret, "from-a-file\r\n").unwrap();
    logins(
        &[],
        &[("GRAPHBUFF_ADMIN_USER", "root"), ("GRAPHBUFF_ADMIN_PASSWORD_FILE", secret.to_str().unwrap())],
        &[("root", "from-a-file", 200), ("admin", "from-a-file", 401)],
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn accounts_are_on_by_default() {
    // Without an administrator, the token holder gets in (and can create one).
    logins(&[], &[("GRAPHBUFF_TOKEN", "tok")], &[("x", "tok", 200), ("", "", 401), ("admin", "x", 401)]);
}

#[test]
fn without_an_administrator_the_server_does_not_start() {
    let dir = temp_dir("no-admin");
    let data = dir.join("data");
    let data = data.to_str().unwrap();
    // In memory: accounts would be lost at every stop, so only the environment helps.
    let err = refused(&[], &[]);
    assert!(err.contains("in-memory accounts are lost"), "{err}");
    // No terminal to ask on (standard input is not one here).
    let err = refused(&["--data", data], &[]);
    assert!(err.contains("no terminal to ask"), "{err}");
    // Users without the admin role do not count.
    users(&["--data", data, "add", "kim", "editor"], &[("GRAPHBUFF_PASSWORD", "kim-password")]).unwrap();
    let err = refused(&["--data", data], &[]);
    assert!(err.contains("no terminal to ask"), "{err}");
    let err = refused(&["--data", data], &[("GRAPHBUFF_ADMIN_USER", "kim"), ("GRAPHBUFF_ADMIN_PASSWORD", "long-enough")]);
    assert!(err.contains("users grant kim admin"), "{err}");
    // Another name from the environment becomes the administrator.
    logins(
        &["--data", data],
        &[("GRAPHBUFF_ADMIN_PASSWORD", "admin-password")],
        &[("admin", "admin-password", 200), ("kim", "kim-password", 200)],
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn accounts_cannot_be_turned_off() {
    let out = command(&["--users", "off"], &[]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option `--users`"));
    // The old variable is ignored: accounts stay on.
    let err = refused(&[], &[("GRAPHBUFF_USERS", "off")]);
    assert!(err.contains("there is no administrator"), "{err}");
}

#[test]
fn the_offline_command_works_on_the_data_directory() {
    let dir = temp_dir("offline");
    let data = dir.join("data");
    let data = data.to_str().unwrap();
    let pw = [("GRAPHBUFF_PASSWORD", "offline-pw")];
    assert_eq!(users(&["--data", data, "add", "root", "admin"], &pw).unwrap().trim(), "added user root");
    users(&["--data", data, "add", "kim", "reader"], &pw).unwrap();
    users(&["--data", data, "grant", "kim", "editor"], &[]).unwrap();
    users(&["--data", data, "remove", "kim"], &[]).unwrap();
    assert_eq!(users(&["--data", data, "list"], &[]).unwrap().trim(), "root\tadmin");
    // GRAPHBUFF_DATA works too.
    assert_eq!(users(&["list"], &[("GRAPHBUFF_DATA", data)]).unwrap().trim(), "root\tadmin");
    logins_while(&["--data", data], &[], &[("root", "offline-pw", 200), ("root", "x", 401)], || {
        // Not while a server uses the data directory.
        let err = users(&["--data", data, "list"], &[]).unwrap_err();
        assert!(err.contains("is a server using it"), "{err}");
    });
    let err = users(&["--data", data, "remove", "ghost"], &[]).unwrap_err();
    assert!(err.contains("does not exist"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_bootstrap_settings_are_refused() {
    let dir = temp_dir("bad");
    let missing = dir.join("missing");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_USER", "root")]);
    assert!(err.contains("GRAPHBUFF_ADMIN_USER needs GRAPHBUFF_ADMIN_PASSWORD"), "{err}");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_PASSWORD", "pw"), ("GRAPHBUFF_ADMIN_PASSWORD_FILE", "x")]);
    assert!(err.contains("not both"), "{err}");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_PASSWORD_FILE", missing.to_str().unwrap())]);
    assert!(err.contains("cannot read GRAPHBUFF_ADMIN_PASSWORD_FILE"), "{err}");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_USER", "bad name!"), ("GRAPHBUFF_ADMIN_PASSWORD", "long-enough")]);
    assert!(err.contains("cannot create the administrator"), "{err}");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_PASSWORD", "short")]);
    assert!(err.contains("at least 8 characters"), "{err}");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_USER", "longname"), ("GRAPHBUFF_ADMIN_PASSWORD", "longname")]);
    assert!(err.contains("differ from the user name"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
