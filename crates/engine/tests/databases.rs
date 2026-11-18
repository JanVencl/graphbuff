//! Multiple databases: CREATE [OR REPLACE] / DROP / SHOW DATABASES, USE, and the catalog.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{
    DEFAULT_DATABASE, DatabaseManager, Error, ExistingDatabase, Session, StorageKind, params,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-dbms-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(s: &mut Session, q: &str) -> Vec<String> {
    s.execute(q)
        .unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect()
}

fn count(s: &mut Session) -> String {
    run(s, "MATCH (n) RETURN count(n)").remove(0)
}

fn names(s: &mut Session) -> Vec<String> {
    run(s, "SHOW DATABASES")
        .into_iter()
        .map(|r| r.split(", ").next().unwrap().trim_matches('"').to_string())
        .collect()
}

#[test]
fn create_use_and_isolation() {
    let mgr = DatabaseManager::in_memory();
    let mut s = mgr.session();
    assert_eq!(s.current_database(), DEFAULT_DATABASE);
    run(&mut s, "CREATE (:InDefault)");

    let r = s.execute("CREATE DATABASE Movies").unwrap();
    assert_eq!(r.stats.system_updates, 1);
    run(&mut s, "USE movies");
    assert_eq!(s.current_database(), "movies");
    assert_eq!(count(&mut s), "0");
    run(
        &mut s,
        "CREATE (:Movie {title: 'Heat'}), (:Movie {title: 'Ronin'})",
    );
    assert_eq!(count(&mut s), "2");

    // USE <db> <query> runs one query elsewhere without switching.
    assert_eq!(
        run(&mut s, "USE graphbuff MATCH (n) RETURN labels(n)"),
        vec![r#"["InDefault"]"#]
    );
    assert_eq!(s.current_database(), "movies");

    assert_eq!(names(&mut s), vec!["graphbuff", "movies"]);
    assert_eq!(
        run(&mut s, "SHOW DATABASE movies"),
        vec![r#""movies", "memory", false, true, 2, 0"#]
    );
    assert_eq!(
        run(&mut s, "SHOW DEFAULT DATABASE"),
        vec![r#""graphbuff", "memory", true, false, 1, 0"#]
    );
    assert!(run(&mut s, "SHOW DATABASE nope").is_empty());
}

#[test]
fn create_variants() {
    let mgr = DatabaseManager::in_memory();
    let mut s = mgr.session();
    run(&mut s, "CREATE DATABASE a");
    run(&mut s, "USE a CREATE (:Old)");

    assert!(matches!(
        s.execute("CREATE DATABASE a"),
        Err(Error::Constraint(_))
    ));
    let r = s.execute("CREATE DATABASE a IF NOT EXISTS").unwrap();
    assert_eq!(r.stats.system_updates, 0);
    assert_eq!(
        run(&mut s, "USE a MATCH (n:Old) RETURN count(n)"),
        vec!["1"]
    );

    // OR REPLACE wipes the existing database...
    let r = s.execute("CREATE OR REPLACE DATABASE a").unwrap();
    assert_eq!(r.stats.system_updates, 1);
    assert_eq!(run(&mut s, "USE a MATCH (n) RETURN count(n)"), vec!["0"]);
    // ...and creates one that did not exist.
    run(&mut s, "CREATE OR REPLACE DATABASE b");
    assert_eq!(names(&mut s), vec!["a", "b", "graphbuff"]);

    // The default database can be replaced too; the session keeps using it.
    run(&mut s, "CREATE (:X)");
    run(&mut s, "CREATE OR REPLACE DATABASE graphbuff");
    assert_eq!(count(&mut s), "0");

    // Names from parameters.
    let mut p = params! { "n" => "fromparam" };
    s.execute_with_params("CREATE DATABASE $n", &p).unwrap();
    p.insert("n".into(), 5.into());
    assert!(matches!(
        s.execute_with_params("CREATE DATABASE $n", &p),
        Err(Error::Type(_))
    ));
}

#[test]
fn drop_database() {
    let mgr = DatabaseManager::in_memory();
    let mut s = mgr.session();
    run(&mut s, "CREATE DATABASE tmp");
    run(&mut s, "USE tmp");
    let r = s.execute("DROP DATABASE tmp").unwrap();
    assert_eq!(r.stats.system_updates, 1);
    // Dropping the current database falls back to the default one.
    assert_eq!(s.current_database(), DEFAULT_DATABASE);
    assert!(matches!(s.execute("USE tmp"), Err(Error::Runtime(_))));
    assert!(matches!(
        s.execute("DROP DATABASE tmp"),
        Err(Error::Runtime(_))
    ));
    assert_eq!(
        s.execute("DROP DATABASE tmp IF EXISTS")
            .unwrap()
            .stats
            .system_updates,
        0
    );
    assert!(matches!(
        s.execute("DROP DATABASE graphbuff"),
        Err(Error::Constraint(_))
    ));

    // Another session that was using a dropped database gets a clear error.
    run(&mut s, "CREATE DATABASE shared");
    let mut other = mgr.session();
    run(&mut other, "USE shared");
    run(&mut s, "DROP DATABASE shared");
    let err = other.execute("RETURN 1").unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
fn options_and_validation() {
    let mgr = DatabaseManager::in_memory();
    let mut s = mgr.session();
    for (q, ok) in [
        ("CREATE DATABASE m OPTIONS {storage: 'memory'}", true),
        ("CREATE DATABASE h OPTIONS {storage: 'hybrid'}", false), // no data directory
        ("CREATE DATABASE p OPTIONS {storage: 'persistent'}", false),
        ("CREATE DATABASE x OPTIONS {storage: 'tape'}", false),
        ("CREATE DATABASE x OPTIONS {colour: 'red'}", false),
        (
            "CREATE DATABASE x OPTIONS {storage: 'memory', sync: 'os'}",
            false,
        ),
        ("CREATE DATABASE `1bad`", false),
        ("CREATE DATABASE `a/../b`", false),
        ("CREATE DATABASE con", false),
    ] {
        assert_eq!(s.execute(q).is_ok(), ok, "{q}");
    }
    assert!(matches!(
        s.execute("CREATE OR REPLACE DATABASE m IF NOT EXISTS"),
        Err(Error::Syntax { .. })
    ));
    assert_eq!(names(&mut s), vec!["graphbuff", "m"]);
}

#[test]
fn catalog_survives_restart() {
    let dir = TempDir::new("restart");
    {
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        let mut s = mgr.session();
        run(&mut s, "CREATE (:Root)");
        run(
            &mut s,
            "CREATE DATABASE movies OPTIONS {storage: 'hybrid', sync: 'os', checkpointAfterBytes: 1024}",
        );
        run(&mut s, "USE movies CREATE (:Movie {title: 'Heat'})");
        run(
            &mut s,
            "CREATE DATABASE scratch OPTIONS {storage: 'memory'}",
        );
        run(&mut s, "USE scratch CREATE (:Gone)");
        run(&mut s, "CREATE DATABASE doomed");
        run(&mut s, "DROP DATABASE doomed");
        assert!(!dir.0.join("databases").join("doomed").exists());

        // A second process cannot open the same directory.
        assert!(matches!(
            DatabaseManager::open(&dir.0),
            Err(Error::Storage(_))
        ));
    }
    let mgr = DatabaseManager::open(&dir.0).unwrap();
    let infos = mgr.databases();
    let storage: Vec<(&str, &StorageKind)> = infos
        .iter()
        .map(|d| (d.name.as_str(), &d.storage))
        .collect();
    assert_eq!(storage.len(), 3);
    assert_eq!(storage[0].0, "graphbuff");
    assert_eq!(
        storage[1],
        (
            "movies",
            &StorageKind::Hybrid {
                sync: graphbuff::SyncPolicy::OsBuffered,
                checkpoint_after_bytes: 1024
            }
        )
    );
    assert_eq!(storage[2], ("scratch", &StorageKind::InMemory));

    let mut s = mgr.session();
    assert_eq!(count(&mut s), "1");
    assert_eq!(
        run(&mut s, "USE movies MATCH (m:Movie) RETURN m.title"),
        vec![r#""Heat""#]
    );
    // Memory databases keep their catalog entry but come back empty.
    assert_eq!(
        run(&mut s, "USE scratch MATCH (n) RETURN count(n)"),
        vec!["0"]
    );
}

#[test]
fn replace_hybrid_database_wipes_files() {
    let dir = TempDir::new("replace");
    {
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        let mut s = mgr.session();
        run(&mut s, "CREATE DATABASE g");
        run(&mut s, "USE g CREATE (:Old)");
        run(&mut s, "CREATE OR REPLACE DATABASE g");
        run(&mut s, "USE g CREATE (:New)");
    }
    let mgr = DatabaseManager::open(&dir.0).unwrap();
    let mut s = mgr.session();
    assert_eq!(
        run(&mut s, "USE g MATCH (n) RETURN labels(n)"),
        vec![r#"["New"]"#]
    );
}

#[test]
fn stale_directory_does_not_leak_into_new_database() {
    let dir = TempDir::new("stale");
    {
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        mgr.create_database("ghost", None, ExistingDatabase::Fail)
            .unwrap();
        mgr.database("ghost")
            .unwrap()
            .execute("CREATE (:Ghost)")
            .unwrap();
    }
    // Simulate a drop whose directory removal failed: remove only the catalog entry.
    let catalog = dir.0.join("catalog");
    let text = fs::read_to_string(&catalog).unwrap();
    let kept: Vec<&str> = text.lines().filter(|l| !l.contains(" ghost ")).collect();
    fs::write(&catalog, kept.join("\n")).unwrap();

    let mgr = DatabaseManager::open(&dir.0).unwrap();
    assert!(mgr.database("ghost").is_err());
    mgr.create_database("ghost", None, ExistingDatabase::Fail)
        .unwrap();
    assert_eq!(mgr.database("ghost").unwrap().node_count(), 0);
}

#[test]
fn max_transaction_bytes_option() {
    let dir = TempDir::new("txcap");
    let catalog = || fs::read_to_string(dir.0.join("catalog")).unwrap();
    {
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        let mut s = mgr.session();
        run(
            &mut s,
            "CREATE DATABASE small OPTIONS {storage: 'persistent', maxTransactionBytes: 262144}",
        );
        run(&mut s, "CREATE DATABASE plain OPTIONS {storage: 'persistent'}");
        for q in [
            "CREATE DATABASE x OPTIONS {storage: 'hybrid', maxTransactionBytes: 1}",
            "CREATE DATABASE x OPTIONS {storage: 'memory', maxTransactionBytes: 1}",
            "CREATE DATABASE x OPTIONS {storage: 'persistent', maxTransactionBytes: -1}",
            "CREATE DATABASE x OPTIONS {storage: 'persistent', maxTransactionBytes: '1'}",
        ] {
            assert!(matches!(s.execute(q), Err(Error::Semantic(_))), "{q}");
        }
        let r = s.execute("USE small UNWIND range(1, 5000) AS i CREATE (:N {i: i, s: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'})");
        assert!(matches!(r, Err(Error::TransactionTooLarge(_))), "{r:?}");
        run(&mut s, "USE plain UNWIND range(1, 5000) AS i CREATE (:N {i: i, s: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'})");
    }
    // Only a changed cap is written, so older versions still read catalogs without one.
    let text = catalog();
    assert!(text.lines().any(|l| l.starts_with("database small ") && l.ends_with(" max_transaction_bytes=262144")), "{text}");
    assert!(text.lines().any(|l| l.starts_with("database plain ") && !l.contains("max_transaction")), "{text}");

    let mgr = DatabaseManager::open(&dir.0).unwrap();
    let small = mgr.databases().into_iter().find(|d| d.name == "small").unwrap();
    assert!(matches!(
        small.storage,
        StorageKind::Persistent { max_transaction_bytes: 262144, .. }
    ));
    let mut s = mgr.session();
    let r = s.execute("USE small UNWIND range(1, 5000) AS i CREATE (:N {i: i, s: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'})");
    assert!(matches!(r, Err(Error::TransactionTooLarge(_))), "{r:?}");
    assert_eq!(run(&mut s, "USE plain MATCH (n) RETURN count(n)"), vec!["5000"]);
}

#[test]
fn single_database_rejects_database_commands() {
    let db = graphbuff::Database::in_memory();
    assert!(matches!(
        db.execute("CREATE DATABASE x"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(db.execute("USE x"), Err(Error::Unsupported(_))));
}

#[test]
fn checkpoint_all_covers_durable_databases() {
    let dir = TempDir::new("checkpoint-all");
    {
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        let mut s = mgr.session();
        run(&mut s, "CREATE DATABASE p OPTIONS {storage: 'persistent'}");
        run(&mut s, "CREATE DATABASE m OPTIONS {storage: 'memory'}");
        run(&mut s, "CREATE (:A)");
        s.use_database("p").unwrap();
        run(&mut s, "CREATE (:B)");
        let wal = dir.0.join("databases/graphbuff/graph.wal");
        let before = fs::metadata(&wal).unwrap().len();

        let mut done: Vec<String> = mgr
            .checkpoint_all()
            .into_iter()
            .map(|(name, r)| {
                r.unwrap_or_else(|e| panic!("{name}: {e}"));
                name
            })
            .collect();
        done.sort();
        if cfg!(feature = "system-db") {
            assert_eq!(done, ["graphbuff", "p", "system"]);
        } else {
            assert_eq!(done, ["graphbuff", "p"]);
        }
        assert!(fs::metadata(&wal).unwrap().len() < before);
    }
    let mgr = DatabaseManager::open(&dir.0).unwrap();
    let mut s = mgr.session();
    assert_eq!(count(&mut s), "1");
    s.use_database("p").unwrap();
    assert_eq!(count(&mut s), "1");
}
