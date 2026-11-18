//! The reserved `system` database: protection rules (every build) and, with the
//! `system-db` feature, its storage.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{DatabaseManager, ExistingDatabase, SYSTEM_DATABASE};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-sysdb-{}-{}-{name}",
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

fn refused(mgr: &DatabaseManager, statement: &str) {
    let mut s = mgr.session();
    match s.execute(statement) {
        Ok(_) => panic!("`{statement}` was accepted"),
        Err(e) => assert!(e.to_string().contains("system"), "`{statement}`: unexpected error {e}"),
    }
}

#[test]
fn the_name_is_reserved() {
    let mgr = DatabaseManager::in_memory();
    for q in [
        "CREATE DATABASE system",
        "CREATE DATABASE SYSTEM",
        "CREATE DATABASE system IF NOT EXISTS",
        "CREATE OR REPLACE DATABASE system",
        "DROP DATABASE system",
        "DROP DATABASE system IF EXISTS",
        "USE system",
        "USE System MATCH (n) RETURN n",
    ] {
        refused(&mgr, q);
    }
    let mut s = mgr.session();
    assert!(s.execute_with_params("USE $db", &[("db".to_string(), graphbuff::Value::String("system".into()))].into()).is_err());
    assert!(mgr.database("system").is_err());
    assert!(mgr.create_database("system", None, ExistingDatabase::Keep).is_err());
    assert!(mgr.drop_database("system", true).is_err());
}

#[test]
fn show_databases_hides_it() {
    let mgr = DatabaseManager::in_memory();
    assert!(mgr.databases().iter().all(|d| d.name != SYSTEM_DATABASE));
    let mut s = mgr.session();
    assert!(s.execute("SHOW DATABASE system").unwrap().rows.is_empty());
    assert_eq!(s.execute("SHOW DATABASES").unwrap().rows.len(), 1);
}

#[test]
fn restore_and_import_into_it_are_refused() {
    let dir = TempDir::new("restore");
    let mgr = DatabaseManager::open(&dir.0).unwrap();
    let err = mgr.restore_database("system", &dir.0, true).unwrap_err();
    assert!(err.to_string().contains("reserved"), "{err}");
    let opts = graphbuff::ImportOptions::default();
    let err = mgr.import_database("system", None, &opts, true).unwrap_err();
    assert!(err.to_string().contains("reserved"), "{err}");
}

#[test]
fn a_catalog_listing_system_is_refused() {
    let dir = TempDir::new("catalog");
    drop(DatabaseManager::open(&dir.0).unwrap());
    let catalog = dir.0.join("catalog");
    let mut text = fs::read_to_string(&catalog).unwrap();
    text.push_str("database system memory\n");
    fs::write(&catalog, text).unwrap();
    let err = DatabaseManager::open(&dir.0).err().expect("opened a catalog with `system`");
    assert!(err.to_string().contains("reserved"), "{err}");
}

#[cfg(not(feature = "system-db"))]
#[test]
fn without_the_feature_there_is_none() {
    assert!(DatabaseManager::in_memory().system_database().is_none());
    let dir = TempDir::new("none");
    assert!(DatabaseManager::open(&dir.0).unwrap().system_database().is_none());
    assert!(!dir.0.join("system").exists());
}

#[cfg(feature = "system-db")]
mod with_feature {
    use super::*;
    use graphbuff::StorageKind;

    #[test]
    fn in_memory_manager_has_one() {
        let mgr = DatabaseManager::in_memory();
        let system = mgr.system_database().expect("system database");
        system.execute("CREATE (:User {name: 'admin'})").unwrap();
        assert_eq!(system.node_count(), 1);
        // Separate from the user databases.
        assert_eq!(mgr.database(&mgr.default_database()).unwrap().node_count(), 0);
    }

    #[test]
    fn it_survives_a_restart_outside_the_catalog() {
        let dir = TempDir::new("restart");
        {
            let mgr = DatabaseManager::open_with(&dir.0, StorageKind::persistent()).unwrap();
            mgr.system_database().unwrap().execute("CREATE (:User {name: 'admin'})").unwrap();
        }
        assert!(dir.0.join("system").is_dir());
        assert!(!dir.0.join("databases/system").exists());
        let catalog = fs::read_to_string(dir.0.join("catalog")).unwrap();
        assert!(!catalog.contains("system"), "{catalog}");

        let mgr = DatabaseManager::open(&dir.0).unwrap();
        let system = mgr.system_database().unwrap();
        let r = system.execute("MATCH (u:User) RETURN u.name").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0].to_string(), "\"admin\"");
    }

    #[test]
    fn a_stale_user_directory_named_system_is_not_used() {
        let dir = TempDir::new("stale");
        fs::create_dir_all(dir.0.join("databases/system")).unwrap();
        fs::write(dir.0.join("databases/system/graph.wal"), b"garbage").unwrap();
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        assert_eq!(mgr.system_database().unwrap().node_count(), 0);
    }

    #[test]
    fn it_can_be_backed_up() {
        let dir = TempDir::new("backup");
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        mgr.system_database().unwrap().execute("CREATE (:User)").unwrap();
        let dest = dir.0.join("backup-system");
        mgr.backup_database("system", &dest).unwrap();
        assert!(dest.join("graph.snapshot").is_file());
    }

    #[test]
    fn the_data_directory_lock_covers_it() {
        let dir = TempDir::new("lock");
        let _mgr = DatabaseManager::open(&dir.0).unwrap();
        let err = DatabaseManager::open(&dir.0).err().expect("second open succeeded");
        assert!(err.to_string().contains("already open"), "{err}");
    }
}
