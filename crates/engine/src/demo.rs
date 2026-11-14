//! Demo graphs compiled into the binaries (`--demo NAME` of `graphbuff` and
//! `graphbuff-server`), so a first run needs no files on disk.

use crate::{DatabaseManager, Error, Result};

/// The names [`load_demo`] accepts.
pub const DEMOS: &[&str] = &["movies"];

/// What [`load_demo`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemoSummary {
    /// False when the default database already held data, so nothing was loaded.
    pub loaded: bool,
    /// Nodes in the default database afterwards.
    pub nodes: u64,
    /// Relationships in the default database afterwards.
    pub relationships: u64,
}

/// The Cypher script of the demo graph `name`.
pub fn demo_script(name: &str) -> Result<&'static str> {
    match name {
        "movies" => Ok(include_str!("../../../examples/movies.cypher")),
        other => Err(Error::Semantic(format!(
            "unknown demo `{other}`; available: {}",
            DEMOS.join(", ")
        ))),
    }
}

/// Loads the demo graph `name` into the default database of `mgr`. A default database
/// that already holds nodes (a `--data` directory from an earlier run) is left as it is.
pub fn load_demo(mgr: &DatabaseManager, name: &str) -> Result<DemoSummary> {
    let script = demo_script(name)?;
    let db = mgr.database(&mgr.default_database())?;
    let counts = || db.read(|s| (s.node_count() as u64, s.relationship_count() as u64));
    let (nodes, relationships) = counts();
    if nodes > 0 {
        return Ok(DemoSummary { loaded: false, nodes, relationships });
    }
    let mut session = mgr.session();
    for statement in crate::cypher::split_statements(script)? {
        session.execute(statement)?;
    }
    let (nodes, relationships) = counts();
    Ok(DemoSummary { loaded: true, nodes, relationships })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movies_demo_loads_once() {
        let mgr = DatabaseManager::in_memory();
        let first = load_demo(&mgr, "movies").unwrap();
        assert_eq!(first, DemoSummary { loaded: true, nodes: 7, relationships: 8 });
        let r = mgr
            .session()
            .execute("MATCH (:Person {name: 'Keanu Reeves'})-[:ACTED_IN]->(m) RETURN count(m) AS n")
            .unwrap();
        assert_eq!(r.get(0, "n").and_then(|v| v.as_int()), Some(3));
        // A second load finds data and adds nothing.
        let second = load_demo(&mgr, "movies").unwrap();
        assert_eq!(second, DemoSummary { loaded: false, nodes: 7, relationships: 8 });
    }

    #[test]
    fn unknown_demo_is_an_error() {
        let mgr = DatabaseManager::in_memory();
        let e = load_demo(&mgr, "nope").unwrap_err().to_string();
        assert!(e.contains("unknown demo `nope`") && e.contains("movies"), "{e}");
    }
}
