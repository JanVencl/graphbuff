//! End-to-end: the `graphbuff-cli` binary against a server on a local port.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use graphbuff::DatabaseManager;
use graphbuff_server::{Server, ServerConfig};

fn server(token: Option<&str>) -> Server {
    Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            token: token.map(str::to_string),
            ..ServerConfig::default()
        },
    )
    .unwrap()
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs the CLI against `server` with `args`, `stdin` as its standard input.
fn cli(server: &Server, args: &[&str], stdin: &str) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphbuff-cli"))
        .arg("--address")
        .arg(format!("http://{}", server.local_addr()))
        .args(args)
        .env_remove("GRAPHBUFF_URL")
        .env_remove("GRAPHBUFF_USER")
        .env_remove("GRAPHBUFF_PASSWORD")
        .env_remove("GRAPHBUFF_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8(o.stdout).unwrap().replace("\r\n", "\n"),
        err: String::from_utf8(o.stderr).unwrap().replace("\r\n", "\n"),
    }
}

#[test]
fn statements_from_the_command_line() {
    let s = server(None);
    let r = cli(&s, &["-P", "n=41", "-P", "who=Ann", "RETURN $n + 1 AS answer, $who AS who"], "");
    assert_eq!((r.code, r.out.as_str()), (0, "answer\twho\n42\tAnn\n"), "{}", r.err);

    let r = cli(&s, &["--format", "csv", "RETURN 'a,b' AS x, [1, 2] AS l, null AS n"], "");
    assert_eq!(r.out, "x,l,n\n\"a,b\",\"[1, 2]\",\n");

    let r = cli(&s, &["--format", "json", "RETURN 1 AS x; RETURN 2 AS y"], "");
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with(r#"{"columns":["x"],"rows":[[1]]"#), "{}", r.out);

    let r = cli(&s, &["--format", "table", "CREATE (:Person {name: 'Ann'})-[:KNOWS]->(:Person) RETURN 1 AS one"], "");
    assert!(r.out.starts_with("+-----+\n| one |\n+-----+\n| 1   |\n+-----+\n1 row, "), "{}", r.out);
    assert!(r.out.contains("Nodes created: 2, Relationships created: 1, Properties set: 1, Labels added: 2"));

    let r = cli(&s, &["MATCH p = (:Person {name: 'Ann'})-->() RETURN p"], "");
    assert_eq!(r.out, "p\n(:Person {name: \"Ann\"})-[:KNOWS]->(:Person)\n");
}

#[test]
fn scripts_with_transactions_and_databases() {
    let s = server(None);
    let script = "\
CREATE DATABASE shop;
:use shop
:begin
CREATE (:Item {n: 1});
CREATE (:Item {n: 2});
:commit
:begin
CREATE (:Item {n: 3});
:rollback
// a comment; not a statement
MATCH (i:Item)
RETURN count(i) AS items;
USE graphbuff;
MATCH (i:Item) RETURN count(i) AS elsewhere
";
    let r = cli(&s, &[], script);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("items\n2\n"), "{}", r.out);
    assert!(r.out.ends_with("elsewhere\n0\n"), "{}", r.out);
    assert!(r.err.contains("transaction begun on database shop"), "{}", r.err);

    // `-d` and `:dbs`.
    let r = cli(&s, &["-d", "shop", "--format", "csv", ":dbs"], "");
    assert!(r.out.starts_with("name,storage,default,nodes,relationships\n"), "{}", r.out);
    assert!(r.out.contains("shop,memory,false,2,0"), "{}", r.out);

    // A transaction left open at the end is rolled back.
    let r = cli(&s, &["-d", "shop"], ":begin\nCREATE (:Item);\n");
    assert!(r.err.contains("the open transaction was rolled back"), "{}", r.err);
    let r = cli(&s, &["-d", "shop", "MATCH (i:Item) RETURN count(i)"], "");
    assert_eq!(r.out, "count(i)\n2\n");

    // :param evaluates its expression on the server.
    let r = cli(&s, &[], ":param ids => range(1, 3)\nUNWIND $ids AS i RETURN sum(i) AS s;");
    assert_eq!(r.out, "$ids = [1, 2, 3]\ns\n6\n", "{}", r.err);
}

#[test]
fn errors() {
    let s = server(None);
    let r = cli(&s, &[], "RETURN 1 AS a;\nRETRUN 2;\nRETURN 3 AS c;\n");
    assert_eq!(r.code, 1);
    assert_eq!(r.out, "a\n1\n", "the script stops at the first error");
    assert!(r.err.starts_with("SyntaxError: "), "{}", r.err);
    assert!(r.err.contains("  RETRUN 2\n  ^\n"), "{}", r.err);

    let r = cli(&s, &["--continue"], "RETURN 1 AS a;\nRETRUN 2;\nRETURN 3 AS c;\n");
    assert_eq!((r.code, r.out.as_str()), (1, "a\n1\nc\n3\n"));

    let r = cli(&s, &["-d", "nope", "RETURN 1"], "");
    assert_eq!(r.code, 1);
    assert!(r.err.contains("DatabaseNotFound"), "{}", r.err);

    let r = cli(&s, &["--bogus"], "");
    assert_eq!(r.code, 2);
}

#[test]
fn token_login() {
    let s = server(Some("s3cret"));
    let r = cli(&s, &["RETURN 1 AS x"], "");
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Unauthorized"), "{}", r.err);
    let r = cli(&s, &["--token", "s3cret", "RETURN 1 AS x"], "");
    assert_eq!((r.code, r.out.as_str()), (0, "x\n1\n"), "{}", r.err);
}

#[test]
fn unreachable_server() {
    let s = server(None);
    let addr = s.local_addr();
    drop(s);
    let r = Command::new(env!("CARGO_BIN_EXE_graphbuff-cli"))
        .args(["--address", &format!("http://{addr}"), "RETURN 1"])
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(1));
}

#[test]
fn user_login() {
    use graphbuff::cypher::ast::ExistingDatabase;
    use graphbuff_server::{HashParams, Role, Users};

    let users = Users::in_memory().with_hash_params(HashParams::TESTING).with_min_password_length(1);
    users.create("rita", "ritapw", &[Role::Reader], ExistingDatabase::Fail).unwrap();
    let s = Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            users: Some(Arc::new(users)),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let r = cli(&s, &["-u", "rita", "-p", "ritapw", "RETURN 1 AS x"], "");
    assert_eq!((r.code, r.out.as_str()), (0, "x\n1\n"), "{}", r.err);
    let r = cli(&s, &["-u", "rita", "-p", "ritapw", "CREATE ()"], "");
    assert_eq!(r.code, 1);
    assert!(r.err.starts_with("Forbidden: "), "{}", r.err);
    let r = cli(&s, &["-u", "rita", "-p", "wrong", "RETURN 1"], "");
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Unauthorized"), "{}", r.err);
    // No password and no terminal to ask on.
    let r = cli(&s, &["-u", "rita", "RETURN 1"], "");
    assert_eq!(r.code, 2);
    assert!(r.err.contains("--password"), "{}", r.err);
}
