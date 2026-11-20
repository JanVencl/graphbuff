//! The real binary and its config file: the data folder and logs it names, what the logs
//! record (start-up, failed logins, slow queries, administrative actions), where the file is looked for and
//! created, and its errors.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;
use common::{toml_path, toml_str};

const BIN: &str = env!("CARGO_BIN_EXE_graphbuff-server");

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphbuff-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Variables that would change where the server looks for its config file or what it
/// does: none of the caller's.
fn clean(cmd: &mut Command) -> &mut Command {
    for key in ["GRAPHBUFF_TOKEN", "GRAPHBUFF_CONFIG", "GRAPHBUFF_ADMIN_USER", "GRAPHBUFF_ADMIN_PASSWORD_FILE"] {
        cmd.env_remove(key);
    }
    cmd.env("GRAPHBUFF_ADMIN_PASSWORD", "admin-password")
}

/// Starts `cmd`; returns it and the address of its first output line.
fn spawn(cmd: &mut Command) -> (Child, String) {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut listening = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut listening).unwrap();
    let addr = listening
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no address in {listening:?}"))
        .to_string();
    (child, addr)
}

/// Starts the server with the config file `config`.
fn start(config: &Path) -> (Child, String) {
    spawn(clean(Command::new(BIN).arg("--config").arg(config)))
}

/// Sends a query; returns the status line.
fn query(addr: &str, auth: Option<&str>, body: &str) -> String {
    let mut conn = TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = auth.map_or(String::new(), |a| format!("Authorization: {a}\r\n"));
    write!(
        conn,
        "POST /query HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut reply = String::new();
    conn.read_to_string(&mut reply).unwrap();
    reply
}

fn stop(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

/// Runs the binary with `args` and a clean environment, expecting it to fail with exit
/// code 2; returns standard error.
fn refused(args: &[&std::ffi::OsStr]) -> String {
    let out = clean(&mut Command::new(BIN)).args(args).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn data_and_logs_go_to_the_data_folder() {
    let data = temp_dir("data-config");
    // Without logs.dir the logs go to DATA/logs.
    let config = common::config_file(&common::toml(&[
        ("server.bind", toml_str("127.0.0.1:0")),
        ("storage.data", toml_path(&data)),
        ("security.token", toml_str("secret")),
        ("logs.slow_query_ms", "0".into()),
    ]));
    let (child, addr) = start(&config);
    let result = std::panic::catch_unwind(|| {
        let ok = query(&addr, Some("Bearer secret"), r#"{"query": "CREATE (:Person {name: 'Ann'})"}"#);
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        let denied = query(&addr, Some("Bearer wrong"), r#"{"query": "RETURN 1"}"#);
        assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
        for admin in [
            "CREATE DATABASE audited",
            "CREATE USER bob SET PASSWORD 'bobs-long-password'",
            "GRANT ROLE editor ON DATABASE audited TO bob",
            "DROP DATABASE nosuch",
        ] {
            query(&addr, Some("Bearer secret"), &format!(r#"{{"query": "{admin}"}}"#));
        }
    });
    stop(child);
    result.unwrap();

    assert!(data.join("catalog").exists(), "no catalog in {}", data.display());
    let events = std::fs::read_to_string(data.join("logs/graphbuff.log")).unwrap();
    assert!(events.contains(" INFO  graphbuff-server "), "{events}");
    assert!(events.contains(&format!("data in {}", data.display())), "{events}");
    assert!(events.contains(&format!("config: {}", config.display())), "{events}");
    assert!(events.contains(" WARN  failed login (http) from 127.0.0.1:"), "{events}");
    let queries = std::fs::read_to_string(data.join("logs/query.log")).unwrap();
    assert!(queries.contains(" http graphbuff token CREATE (:Person {name: 'Ann'})"), "{queries}");
    assert!(!queries.contains("RETURN 1"), "{queries}");
    let security = std::fs::read_to_string(data.join("logs/security.log")).unwrap();
    for line in [
        " AUDIT http 127.0.0.1:",
        " - LOGIN: failed",
        " token CREATE DATABASE audited: ok",
        " token CREATE USER bob: ok",
        " token GRANT ROLE editor ON DATABASE audited TO bob: ok",
        " token DROP DATABASE nosuch: failed: ",
    ] {
        assert!(security.contains(line), "{line:?} not in {security}");
    }
    assert!(!security.contains("bobs-long-password") && !queries.contains("bobs-long-password"), "{security}");
    assert!(!security.contains("CREATE (:Person"), "{security}");

    // The data is still there after a restart; GRAPHBUFF_TOKEN overrides the file's token.
    let (child, addr) = spawn(clean(Command::new(BIN).arg("--config").arg(&config)).env("GRAPHBUFF_TOKEN", "from-env"));
    let result = std::panic::catch_unwind(|| {
        let reply = query(&addr, Some("Bearer from-env"), r#"{"query": "MATCH (p:Person) RETURN p.name AS name"}"#);
        assert!(reply.contains("Ann"), "{reply}");
        let old = query(&addr, Some("Bearer secret"), r#"{"query": "RETURN 1"}"#);
        assert!(old.starts_with("HTTP/1.1 401"), "{old}");
    });
    stop(child);
    result.unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn paths_are_relative_to_the_file_and_memory_mode_writes_no_files() {
    let dir = temp_dir("relative");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("graphbuff.toml");
    std::fs::write(
        &config,
        "[server]\nbind = \"127.0.0.1:0\"\n\n[storage]\ndata = \"db\"   # relative to this file\n\n\
         [logs]\ndir = \"mylogs\"\nlevel = \"warn\"\n",
    )
    .unwrap();
    let (child, _) = start(&config);
    stop(child);
    assert!(dir.join("db").join("catalog").exists());
    assert!(!dir.join("db").join("logs").exists());
    // At level warn, the start-up lines are not written.
    assert_eq!(std::fs::read_to_string(dir.join("mylogs").join("graphbuff.log")).unwrap(), "");

    let memory = dir.join("memory.toml");
    std::fs::write(&memory, "[server]\nbind = \"127.0.0.1:0\"\n[storage]\nmode = \"memory\"\n").unwrap();
    let before: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    let (child, _) = start(&memory);
    stop(child);
    let after: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before.len(), after.len(), "memory mode wrote files");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The layout of the release archives: the program, and its config file in `config/`
/// next to it. Started without one, the program creates it there from the template; with
/// it, the data folder is `data/` beside the program.
#[test]
fn the_config_file_next_to_the_program_is_created_and_used() {
    let dir = temp_dir("portable");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join(Path::new(BIN).file_name().unwrap());
    std::fs::copy(BIN, &exe).unwrap();
    // No per-user config file either: the per-user folders point into the test folder.
    let home = dir.join("home");
    let program = || {
        let mut cmd = Command::new(&exe);
        clean(&mut cmd)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("XDG_DATA_HOME", &home)
            .env("LOCALAPPDATA", &home);
        cmd
    };
    // The first start creates the file (whether or not port 7475 is free here).
    let out = program().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let file = dir.join("config").join("graphbuff.toml");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !file.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    stop(out);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("[storage]") && text.contains("data = \"../data\""), "{text}");
    assert!(!home.join("graphbuff").exists(), "no per-user file when the program's folder is writable");
    // Used: on a free port, with the data folder next to the program.
    std::fs::write(&file, text.replace("bind = \"127.0.0.1:7475\"", "bind = \"127.0.0.1:0\"")).unwrap();
    let _ = std::fs::remove_dir_all(dir.join("data"));
    let (child, _) = spawn(&mut program());
    stop(child);
    assert!(dir.join("data").join("catalog").exists());
    assert!(dir.join("data").join("logs").join("graphbuff.log").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_config_files_and_old_options_are_refused() {
    let dir = temp_dir("bad-config");
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.toml");
    std::fs::write(&bad, "[server]\nnope = 1\n").unwrap();
    let err = refused(&["--config".as_ref(), bad.as_os_str()]);
    assert!(err.contains("bad.toml:2: unknown setting `nope` in [server]"), "{err}");
    std::fs::write(&bad, "[storage]\nmode = \"memory\"\ndata = \"x\"\n").unwrap();
    let err = refused(&["--config".as_ref(), bad.as_os_str()]);
    assert!(err.contains("keeps no data on disk"), "{err}");
    // A file named on the command line must exist.
    let missing = dir.join("missing.toml");
    let err = refused(&["--config".as_ref(), missing.as_os_str()]);
    assert!(err.contains("cannot read the config file"), "{err}");
    // The old command-line options name their setting.
    let err = refused(&["--in-memory".as_ref()]);
    assert!(err.contains("`--in-memory` is gone") && err.contains("mode = \"memory\" in [storage]"), "{err}");
    let err = refused(&["--bind".as_ref(), "0.0.0.0:7475".as_ref()]);
    assert!(err.contains("set bind in [server]"), "{err}");
    let err = refused(&["--nope".as_ref()]);
    assert!(err.contains("unknown option `--nope`"), "{err}");
    let err = refused(&["--config".as_ref(), "-".as_ref(), "extra".as_ref()]);
    assert!(err.contains("unexpected `extra`"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
