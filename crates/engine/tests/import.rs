//! Bulk CSV import (`import_csv`, `DatabaseManager::import_database`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{
    DatabaseManager, Error, IdType, IndexEntity, ImportFiles, ImportOptions, PersistentGraph, PersistentOptions, Session,
    StorageKind, import_csv, parse_index,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-import-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.0.join(name);
        fs::write(&p, text).unwrap();
        p
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
        .map(|r| r.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        .collect()
}

fn group(labels: &[&str], files: &[&Path]) -> ImportFiles {
    ImportFiles {
        labels: labels.iter().map(|s| s.to_string()).collect(),
        files: files.iter().map(|p| p.to_path_buf()).collect(),
    }
}

/// Movies: two id spaces, a header in its own file, typed and array properties.
fn movies(t: &TempDir) -> ImportOptions {
    let people_header = t.file("people_header.csv", "personId:ID(Person),name,born:int,tags:string[],:LABEL\n");
    let people1 = t.file(
        "people1.csv",
        "p1,Keanu Reeves,1964,actor;producer,Actor\n\
         p2,\"Carrie-Anne Moss\",1967,,Actor\n",
    );
    let people2 = t.file("people2.csv", "p3,Lana Wachowski,1965,director,Director;Writer\np4,\"\",,,\n");
    let movies = t.file(
        "movies.csv",
        "movieId:ID(Movie),title,released:date,rating:float,location:point{crs:WGS-84},runtime:duration,:IGNORE\n\
         m1,The Matrix,1999-03-31,8.7,\"{latitude: 33.9, longitude: -118.4}\",PT2H16M,junk\n\
         m2,\"The Matrix, Reloaded\",2003-05-15,7.2,,,junk\n",
    );
    let acted = t.file(
        "acted.csv",
        ":START_ID(Person),:END_ID(Movie),roles:string[],paid:boolean\n\
         p1,m1,Neo,true\n\
         p2,m1,Trinity,false\n\
         p1,m2,Neo;The One,\n",
    );
    let other = t.file(
        "other.csv",
        ":START_ID(Person),:END_ID(Movie),:TYPE,since:datetime\n\
         p3,m1,DIRECTED,1998-01-01T10:00:00Z\n\
         p3,m2,DIRECTED,\n\
         p1,p2,KNOWS_WRONG_SPACE,\n",
    );
    ImportOptions {
        nodes: vec![
            group(&["Person"], &[&people_header, &people1, &people2]),
            group(&["Movie"], &[&movies]),
        ],
        relationships: vec![group(&["ACTED_IN"], &[&acted]), group(&[], &[&other])],
        skip_bad_relationships: true,
        indexes: vec![parse_index("Person(name)").unwrap(), parse_index("[:ACTED_IN](paid)").unwrap()],
        ..ImportOptions::default()
    }
}

#[test]
fn imports_a_database_into_the_catalog() {
    let t = TempDir::new("movies");
    let opts = movies(&t);
    let data = t.0.join("data");
    let summary = {
        let mgr = DatabaseManager::open(&data).unwrap();
        mgr.import_database("movies", None, &opts, false).unwrap()
    };
    assert_eq!((summary.nodes, summary.relationships), (6, 5));
    assert_eq!(summary.skipped_relationships, 1);
    assert_eq!(summary.indexes, 2);

    // Reopened like the CLI or server would.
    let mgr = DatabaseManager::open(&data).unwrap();
    let info = mgr.databases().into_iter().find(|d| d.name == "movies").unwrap();
    assert_eq!(info.storage.name(), "persistent");
    let mut s = mgr.session();
    run(&mut s, "USE movies");
    assert_eq!(
        run(&mut s, "MATCH (p:Person) RETURN p.personId, p.name, p.born, p.tags, labels(p) ORDER BY p.personId"),
        [
            "\"p1\", \"Keanu Reeves\", 1964, [\"actor\", \"producer\"], [\"Person\", \"Actor\"]",
            "\"p2\", \"Carrie-Anne Moss\", 1967, null, [\"Person\", \"Actor\"]",
            "\"p3\", \"Lana Wachowski\", 1965, [\"director\"], [\"Person\", \"Director\", \"Writer\"]",
            "\"p4\", \"\", null, null, [\"Person\"]",
        ]
    );
    assert_eq!(
        run(
            &mut s,
            "MATCH (m:Movie) RETURN m.title, m.released, m.rating, m.location.latitude, m.runtime, keys(m) ORDER BY m.title"
        ),
        [
            "\"The Matrix\", 1999-03-31, 8.7, 33.9, PT2H16M, [\"location\", \"movieId\", \"rating\", \"released\", \"runtime\", \"title\"]",
            "\"The Matrix, Reloaded\", 2003-05-15, 7.2, null, null, [\"movieId\", \"rating\", \"released\", \"title\"]",
        ]
    );
    assert_eq!(
        run(&mut s, "MATCH (p)-[r:ACTED_IN]->(m) RETURN p.name, r.roles, r.paid, m.title ORDER BY p.name, m.title"),
        [
            "\"Carrie-Anne Moss\", [\"Trinity\"], false, \"The Matrix\"",
            "\"Keanu Reeves\", [\"Neo\"], true, \"The Matrix\"",
            "\"Keanu Reeves\", [\"Neo\", \"The One\"], null, \"The Matrix, Reloaded\"",
        ]
    );
    assert_eq!(
        run(&mut s, "MATCH (:Director)-[d:DIRECTED]->(m) RETURN m.title, d.since ORDER BY m.title"),
        ["\"The Matrix\", 1998-01-01T10:00:00Z", "\"The Matrix, Reloaded\", null"]
    );
    // The indexes exist and are used.
    let explained = s
        .execute("EXPLAIN MATCH (p:Person {name: 'Keanu Reeves'}) RETURN p.born")
        .unwrap()
        .plan
        .map(|p| format!("{p:?}"))
        .unwrap_or_default();
    assert!(explained.contains("NodeIndexSeek"), "{explained}");
    assert_eq!(run(&mut s, "MATCH ()-[r:ACTED_IN {paid: true}]->() RETURN count(r)"), ["1"]);
    assert_eq!(run(&mut s, "SHOW INDEXES").len(), 2);
    // And it takes ordinary writes.
    run(&mut s, "CREATE (:Person {name: 'Hugo Weaving'})");
    assert_eq!(run(&mut s, "MATCH (p:Person {name: 'Hugo Weaving'}) RETURN count(p)"), ["1"]);
}

#[test]
fn replaces_only_empty_databases_unless_told() {
    let t = TempDir::new("replace");
    let data = t.0.join("data");
    let nodes = t.file("n.csv", "id:ID,:LABEL\na,A\nb,B\n");
    let opts = ImportOptions {
        nodes: vec![group(&[], &[&nodes])],
        ..ImportOptions::default()
    };
    let mgr = DatabaseManager::open_with(&data, StorageKind::hybrid()).unwrap();
    // The default database exists but is empty: replaced, now persistent.
    let default = mgr.default_database();
    mgr.import_database(&default, None, &opts, false).unwrap();
    let mut s = mgr.session();
    assert_eq!(run(&mut s, "MATCH (n) RETURN count(n)"), ["2"]);
    let e = mgr.import_database(&default, None, &opts, false).unwrap_err();
    assert!(matches!(e, Error::Constraint(_)), "{e}");
    drop(s);
    let more = t.file("m.csv", "id:ID\nx\ny\nz\n");
    let opts = ImportOptions {
        nodes: vec![group(&["X"], &[&more])],
        ..ImportOptions::default()
    };
    mgr.import_database(&default, None, &opts, true).unwrap();
    let mut s = mgr.session();
    assert_eq!(run(&mut s, "MATCH (n:X) RETURN count(n)"), ["3"]);
    assert_eq!(run(&mut s, "MATCH (n) RETURN count(n)"), ["3"]);

    // A failed import leaves the database as it was, and no files behind.
    let bad = t.file("bad.csv", "id:ID,age:int\nq,notanumber\n");
    let opts = ImportOptions {
        nodes: vec![group(&[], &[&bad])],
        ..ImportOptions::default()
    };
    let e = mgr.import_database(&default, None, &opts, true).unwrap_err();
    assert!(e.to_string().contains("bad.csv line 2: field `age`: `notanumber` is not an integer"), "{e}");
    assert_eq!(run(&mut s, "MATCH (n) RETURN count(n)"), ["3"]);
    let leftovers: Vec<_> = fs::read_dir(data.join("databases"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, [default]);
}

fn load(dir: &Path, opts: &ImportOptions) -> graphbuff::Result<PersistentGraph> {
    let mut loader = PersistentGraph::bulk_loader(PersistentOptions::new(dir))?;
    import_csv(&mut loader, opts)?;
    loader.finish()?;
    PersistentGraph::open(PersistentOptions::new(dir))
}

#[test]
fn errors_name_the_file_and_line() {
    let t = TempDir::new("errors");
    let cases: &[(&str, &str, &str)] = &[
        ("dup.csv", "id:ID\na\n\nb\na\n", "dup.csv line 5: duplicate node id `a`"),
        ("type.csv", "id:ID,x:float\na,1.5\nb,x\n", "type.csv line 3: field `x`: `x` is not a number"),
        ("hdr.csv", "id:ID,x:widget\n", "hdr.csv line 1: header field `x:widget`: unknown type `widget`"),
        ("rel.csv", "id:ID,:START_ID\n", "`:START_ID` does not belong in a node file"),
        ("two.csv", "a:ID,b:ID\n", "more than one :ID column"),
        ("pt.csv", "id:ID,p:point\na,\"{x: 1}\"\n", "pt.csv line 2: field `p`"),
    ];
    for (i, (name, text, want)) in cases.iter().enumerate() {
        let f = t.file(name, text);
        let opts = ImportOptions {
            nodes: vec![group(&[], &[&f])],
            ..ImportOptions::default()
        };
        let e = load(&t.0.join(format!("store{i}")), &opts).unwrap_err().to_string();
        assert!(e.contains(want), "{name}: {e}");
    }

    let nodes = t.file("nodes.csv", "id:ID\na\nb\n");
    let rels = t.file("rels.csv", ":START_ID,:END_ID\na,b\na,zz\n");
    let mut opts = ImportOptions {
        nodes: vec![group(&[], &[&nodes])],
        relationships: vec![group(&["R"], &[&rels])],
        ..ImportOptions::default()
    };
    let e = load(&t.0.join("rels"), &opts).unwrap_err().to_string();
    assert!(e.contains("rels.csv line 3: node id `zz` is not found"), "{e}");
    let untyped = t.file("untyped.csv", ":START_ID,:END_ID\na,b\n");
    opts.relationships = vec![group(&[], &[&untyped])];
    let e = load(&t.0.join("untyped"), &opts).unwrap_err().to_string();
    assert!(e.contains("untyped.csv line 2: the relationship has no type"), "{e}");
}

#[test]
fn integer_ids_tabs_and_skipping_duplicates() {
    let t = TempDir::new("ints");
    let nodes = t.file("n.tsv", "uid:ID\tname\n1\tone\n007\tseven\n7\tdup\n");
    let rels = t.file("r.tsv", ":START_ID\t:END_ID\t:TYPE\n01\t7\tLINK\n");
    let opts = ImportOptions {
        nodes: vec![group(&["N"], &[&nodes])],
        relationships: vec![group(&[], &[&rels])],
        delimiter: b'\t',
        id_type: IdType::Integer,
        skip_duplicate_nodes: true,
        ..ImportOptions::default()
    };
    let dir = t.0.join("store");
    let mut loader = PersistentGraph::bulk_loader(PersistentOptions::new(&dir)).unwrap();
    let sum = import_csv(&mut loader, &opts).unwrap();
    loader.finish().unwrap();
    assert_eq!((sum.nodes, sum.skipped_nodes, sum.relationships), (2, 1, 1));
    let db = graphbuff::Database::open(graphbuff::StorageMode::Persistent(PersistentOptions::new(&dir))).unwrap();
    let rows = db.execute("MATCH (a)-[:LINK]->(b) RETURN a.uid, a.name, b.uid, b.name").unwrap().rows;
    assert_eq!(rows.len(), 1);
    let row: Vec<String> = rows[0].iter().map(ToString::to_string).collect();
    assert_eq!(row, ["1", "\"one\"", "7", "\"seven\""]);
}

#[test]
fn parses_index_specs() {
    let ix = parse_index("Person(name)").unwrap();
    assert_eq!((ix.name.as_str(), ix.label.as_str()), ("index_Person_name", "Person"));
    let ix = parse_index(":P(a, b)").unwrap();
    assert_eq!(ix.properties, ["a", "b"]);
    let ix = parse_index("[:KNOWS](since)").unwrap();
    assert_eq!(ix.entity, IndexEntity::Relationship);
    assert!(parse_index("Person").is_err());
    assert!(parse_index("Person()").is_err());
}
