//! The real binary: the first administrator from GRAPHBUFF_ADMIN_USER and
//! GRAPHBUFF_ADMIN_PASSWORD (or _FILE) when the users file is empty, and GRAPHBUFF_USERS.

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
    "GRAPHBUFF_USERS",
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

fn command(args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(["--bind", "127.0.0.1:0", "--in-memory"]).args(args);
    cmd.env("GRAPHBUFF_CONFIG", "-");
    for key in VARS {
        cmd.env_remove(key);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

/// Starts the server, checks each `(user, password, expected status)` login over HTTP,
/// and stops it.
fn logins(args: &[&str], env: &[(&str, &str)], checks: &[(&str, &str, u16)]) {
    let mut child = command(args, env).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
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
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn the_first_start_creates_the_administrator_and_later_starts_keep_it() {
    let dir = temp_dir("first");
    let users = dir.join("users.txt");
    let users = users.to_str().unwrap();
    logins(
        &["--users", users],
        &[("GRAPHBUFF_ADMIN_PASSWORD", "first-pw")],
        &[("admin", "first-pw", 200), ("admin", "wrong", 401), ("", "", 401)],
    );
    // The file has an account now: a different password in the environment is ignored.
    logins(
        &["--users", users],
        &[("GRAPHBUFF_ADMIN_PASSWORD", "second-pw")],
        &[("admin", "first-pw", 200), ("admin", "second-pw", 401)],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(users).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn password_file_user_name_and_users_from_the_environment() {
    let dir = temp_dir("file");
    let secret = dir.join("secret");
    std::fs::write(&secret, "from-a-file\r\n").unwrap();
    let users = dir.join("users.txt");
    logins(
        &[],
        &[
            ("GRAPHBUFF_USERS", users.to_str().unwrap()),
            ("GRAPHBUFF_ADMIN_USER", "root"),
            ("GRAPHBUFF_ADMIN_PASSWORD_FILE", secret.to_str().unwrap()),
        ],
        &[("root", "from-a-file", 200), ("admin", "from-a-file", 401)],
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn users_dash_turns_accounts_off() {
    // The token is the only login.
    logins(&["--users", "-"], &[("GRAPHBUFF_TOKEN", "tok")], &[("x", "tok", 200), ("", "", 401)]);
}

#[test]
fn bad_bootstrap_settings_are_refused() {
    let dir = temp_dir("bad");
    let users = dir.join("users.txt");
    let users = users.to_str().unwrap();
    let missing = dir.join("missing");
    let err = refused(&[], &[("GRAPHBUFF_ADMIN_PASSWORD", "pw")]);
    assert!(err.contains("needs a users file"), "{err}");
    let err = refused(&["--users", users], &[("GRAPHBUFF_ADMIN_USER", "root")]);
    assert!(err.contains("GRAPHBUFF_ADMIN_USER needs GRAPHBUFF_ADMIN_PASSWORD"), "{err}");
    let err = refused(
        &["--users", users],
        &[("GRAPHBUFF_ADMIN_PASSWORD", "pw"), ("GRAPHBUFF_ADMIN_PASSWORD_FILE", "x")],
    );
    assert!(err.contains("not both"), "{err}");
    let err = refused(&["--users", users], &[("GRAPHBUFF_ADMIN_PASSWORD_FILE", missing.to_str().unwrap())]);
    assert!(err.contains("cannot read GRAPHBUFF_ADMIN_PASSWORD_FILE"), "{err}");
    let err = refused(&["--users", users], &[("GRAPHBUFF_ADMIN_USER", "bad name!"), ("GRAPHBUFF_ADMIN_PASSWORD", "pw")]);
    assert!(err.contains("cannot create the administrator"), "{err}");
    assert!(!std::path::Path::new(users).exists());
    let _ = std::fs::remove_dir_all(&dir);
}
