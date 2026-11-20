//! Bulk CSV import into a new persistent store, in the file format of `neo4j-admin
//! database import`.
//!
//! Inputs come in groups ([`ImportFiles`]): the first record of a group's first file is
//! its header, and the other files of the group have no header. A header names one
//! column per field:
//!
//! ```text
//! personId:ID(Person),name,born:int,tags:string[],:LABEL      node files
//! :START_ID(Person),:END_ID(Movie),:TYPE,roles:string[]        relationship files
//! ```
//!
//! * `name:ID(space)` / `:ID(space)`: the node's import id, unique within its id space
//!   (one global space when `(space)` is left out). With a name, the id is also stored
//!   as that property.
//! * `:START_ID(space)`, `:END_ID(space)`: the ends of a relationship.
//! * `:LABEL`: labels, separated by the array delimiter; `:TYPE`: the relationship type.
//!   A group may give labels or a type for all its records too.
//! * `name:type` a property: `int`, `long`, `short`, `byte`, `float`, `double`,
//!   `boolean`, `string`, `char`, `point`, `date`, `localtime`, `time`, `localdatetime`,
//!   `datetime`, `duration`, or an array of one of them (`int[]`, split on the array
//!   delimiter). A plain `name` is a string.
//! * `:IGNORE`: a column that is skipped.
//!
//! An empty unquoted field (and an empty quoted one, except for strings) sets no
//! property. Points are written as maps, `{x: 1, y: 2}` or `{latitude: 52.1, longitude:
//! 4.3, crs: 'WGS-84'}`; `point{crs:WGS-84}` sets a default CRS for a column. Temporal
//! values are ISO 8601 text.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use graphbuff_core::{BulkLoader, IndexDef, IndexEntity, NodeId, Point, Temporal, TemporalKind};

use crate::csv::{CsvReader, Field};
use crate::{Error, Result, Value};

/// One group of input files: a header in the first record of the first file, and the
/// labels (node files) or the type (relationship files, the first entry) every record
/// gets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportFiles {
    pub labels: Vec<String>,
    pub files: Vec<PathBuf>,
}

/// How node ids in the files are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IdType {
    /// Any text; stored as a string property when the column is named.
    #[default]
    String,
    /// Integers (`007` and `7` are the same id); stored as integer properties.
    Integer,
}

/// What [`import_csv`] reads, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportOptions {
    pub nodes: Vec<ImportFiles>,
    pub relationships: Vec<ImportFiles>,
    /// Field delimiter (default `,`).
    pub delimiter: u8,
    /// Separator of array elements and of labels (default `;`).
    pub array_delimiter: char,
    pub id_type: IdType,
    /// Skip relationships whose ends are not found, or that have no type, instead of
    /// failing.
    pub skip_bad_relationships: bool,
    /// Skip nodes whose id was seen before in their id space, instead of failing.
    pub skip_duplicate_nodes: bool,
    /// Property indexes built after loading.
    pub indexes: Vec<IndexDef>,
    /// Page cache of the store while importing ([`DatabaseManager::import_database`](crate::DatabaseManager::import_database);
    /// default 512 MiB). Larger caches write pages back less often.
    pub cache_bytes: u64,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            nodes: Vec::new(),
            relationships: Vec::new(),
            delimiter: b',',
            array_delimiter: ';',
            id_type: IdType::String,
            skip_bad_relationships: false,
            skip_duplicate_nodes: false,
            indexes: Vec::new(),
            cache_bytes: 512 << 20,
        }
    }
}

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub nodes: u64,
    pub relationships: u64,
    pub properties: u64,
    pub labels: u64,
    pub skipped_nodes: u64,
    pub skipped_relationships: u64,
    pub indexes: u64,
}

/// Parses an index given as `Label(a, b)` / `:Label(a)` (a node index) or `[:TYPE](a)`
/// (a relationship index), named as `CREATE INDEX` without a name would name it.
pub fn parse_index(spec: &str) -> Result<IndexDef> {
    let bad = || Error::Runtime(format!("invalid index `{spec}`: expected `Label(prop, …)` or `[:TYPE](prop, …)`"));
    let s = spec.trim();
    let (target, props) = s.strip_suffix(')').and_then(|s| s.split_once('(')).ok_or_else(bad)?;
    let (entity, label) = match target.trim().strip_prefix("[:").and_then(|t| t.strip_suffix(']')) {
        Some(t) => (IndexEntity::Relationship, t),
        None => (IndexEntity::Node, target.trim().trim_start_matches(':')),
    };
    let label = label.trim().trim_matches('`');
    let properties: Vec<String> = props.split(',').map(|p| p.trim().trim_matches('`').to_string()).collect();
    if label.is_empty() || properties.iter().any(String::is_empty) {
        return Err(bad());
    }
    Ok(IndexDef {
        name: format!("index_{label}_{}", properties.join("_")),
        entity,
        label: label.to_string(),
        properties,
        extra_labels: Vec::new(),
        kind: Default::default(),
    })
}

// ---- headers ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scalar {
    Int,
    Float,
    Bool,
    String,
    Point,
    Temporal(TemporalKind),
}

#[derive(Clone, Debug)]
struct PropCol {
    name: String,
    ty: Scalar,
    array: bool,
    /// `point{crs:…}`: the CRS of values that name none.
    crs: Option<String>,
}

#[derive(Clone, Debug)]
enum Column {
    Ignore,
    /// A node's id: its space, and the property it is also stored as.
    Id { space: String, prop: Option<String> },
    Label,
    Type,
    StartId(String),
    EndId(String),
    Prop(PropCol),
}

fn scalar(name: &str) -> Option<Scalar> {
    Some(match name.to_ascii_lowercase().as_str() {
        "int" | "long" | "short" | "byte" => Scalar::Int,
        "float" | "double" => Scalar::Float,
        "boolean" => Scalar::Bool,
        "string" | "char" => Scalar::String,
        "point" => Scalar::Point,
        "date" => Scalar::Temporal(TemporalKind::Date),
        "localtime" => Scalar::Temporal(TemporalKind::LocalTime),
        "time" => Scalar::Temporal(TemporalKind::Time),
        "localdatetime" => Scalar::Temporal(TemporalKind::LocalDateTime),
        "datetime" => Scalar::Temporal(TemporalKind::DateTime),
        "duration" => Scalar::Temporal(TemporalKind::Duration),
        _ => return None,
    })
}

/// `ID(space)` → (`ID`, `space`).
fn split_space(ty: &str) -> (&str, String) {
    match ty.strip_suffix(')').and_then(|t| t.split_once('(')) {
        Some((t, space)) => (t, space.to_string()),
        None => (ty, String::new()),
    }
}

/// Parses one header field. `nodes` says which kind of file it is in.
fn column(field: &str, nodes: bool, at: &str) -> Result<Column> {
    let bad = |why: String| Error::Runtime(format!("{at}: header field `{field}`: {why}"));
    let (name, ty) = match field.split_once(':') {
        Some((n, t)) => (n.trim(), t.trim()),
        None => (field.trim(), "string"),
    };
    let (ty, options) = match ty.strip_suffix('}').and_then(|t| t.split_once('{')) {
        Some((t, o)) => (t, Some(o)),
        None => (ty, None),
    };
    let (base, space) = split_space(ty);
    let upper = base.to_ascii_uppercase();
    let structural = matches!(upper.as_str(), "ID" | "START_ID" | "END_ID" | "LABEL" | "TYPE" | "IGNORE");
    if structural {
        if options.is_some() {
            return Err(bad("options are only supported for point columns".into()));
        }
        if !space.is_empty() && !matches!(upper.as_str(), "ID" | "START_ID" | "END_ID") {
            return Err(bad("only id columns have an id space".into()));
        }
        let col = match upper.as_str() {
            "IGNORE" => Column::Ignore,
            "ID" => Column::Id {
                space,
                prop: (!name.is_empty()).then(|| name.to_string()),
            },
            "LABEL" => Column::Label,
            "TYPE" => Column::Type,
            "START_ID" => Column::StartId(space),
            _ => Column::EndId(space),
        };
        let fits = match col {
            Column::Ignore => true,
            Column::Id { .. } | Column::Label => nodes,
            _ => !nodes,
        };
        if !fits {
            let kind = if nodes { "node" } else { "relationship" };
            return Err(bad(format!("`:{upper}` does not belong in a {kind} file")));
        }
        return Ok(col);
    }
    if name.is_empty() {
        return Err(bad("a property column needs a name".into()));
    }
    let (elem, array) = match ty.strip_suffix("[]") {
        Some(e) => (e, true),
        None => (base, false),
    };
    let ty = scalar(elem).ok_or_else(|| bad(format!("unknown type `{elem}`")))?;
    let mut crs = None;
    if let Some(o) = options {
        if ty != Scalar::Point {
            return Err(bad("options are only supported for point columns".into()));
        }
        for opt in o.split(',') {
            match opt.split_once(':') {
                Some((k, v)) if k.trim().eq_ignore_ascii_case("crs") => {
                    crs = Some(v.trim().trim_matches(['\'', '"']).to_string());
                }
                _ => return Err(bad(format!("unsupported option `{}`", opt.trim()))),
            }
        }
    }
    Ok(Column::Prop(PropCol {
        name: name.to_string(),
        ty,
        array,
        crs,
    }))
}

// ---- values -------------------------------------------------------------------------------

fn parse_scalar(text: &str, col: &PropCol) -> std::result::Result<Value, String> {
    let t = text.trim();
    match col.ty {
        Scalar::String => Ok(Value::String(text.to_string())),
        Scalar::Int => t.parse::<i64>().map(Value::Int).map_err(|_| format!("`{t}` is not an integer")),
        Scalar::Float => t.parse::<f64>().map(Value::Float).map_err(|_| format!("`{t}` is not a number")),
        Scalar::Bool => match t.to_ascii_lowercase().as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("`{t}` is not a boolean")),
        },
        Scalar::Temporal(kind) => Temporal::parse(kind, t).map(Value::Temporal).map_err(|e| e.to_string()),
        Scalar::Point => {
            let mut map = parse_map(t)?;
            if let Some(crs) = &col.crs
                && !map.contains_key("crs")
                && !map.contains_key("srid")
            {
                map.insert("crs".into(), Value::String(crs.clone()));
            }
            match Point::from_map(&map) {
                Ok(Some(p)) => Ok(Value::Point(p)),
                Ok(None) => Ok(Value::Null),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

/// A map of numbers and strings: `{x: 1, y: -2.5, crs: 'cartesian'}`.
fn parse_map(text: &str) -> std::result::Result<BTreeMap<String, Value>, String> {
    let bad = || format!("`{text}` is not a point map like {{x: 1, y: 2}}");
    let inner = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).ok_or_else(bad)?;
    let mut map = BTreeMap::new();
    for entry in inner.split(',').filter(|e| !e.trim().is_empty()) {
        let (k, v) = entry.split_once(':').ok_or_else(bad)?;
        let k = k.trim().trim_matches(['\'', '"', '`']).to_string();
        let v = v.trim();
        let value = if let Some(s) = v
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .or_else(|| v.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        {
            Value::String(s.to_string())
        } else if let Ok(i) = v.parse::<i64>() {
            Value::Int(i)
        } else if let Ok(f) = v.parse::<f64>() {
            Value::Float(f)
        } else {
            Value::String(v.to_string())
        };
        map.insert(k, value);
    }
    Ok(map)
}

/// The value of a property field, or `None` for no property.
fn field_value(field: &Field, col: &PropCol, array_delimiter: char) -> std::result::Result<Option<Value>, String> {
    let (text, quoted) = field;
    if text.is_empty() && !(*quoted && col.ty == Scalar::String && !col.array) {
        return Ok(None);
    }
    if col.array {
        let items = text
            .split(array_delimiter)
            .map(|item| parse_scalar(item, col))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Ok(Some(Value::List(items)));
    }
    let v = parse_scalar(text, col)?;
    Ok((!v.is_null()).then_some(v))
}

// ---- reading groups -----------------------------------------------------------------------

/// The records of one input group, header first.
struct Group<'a> {
    files: std::slice::Iter<'a, PathBuf>,
    reader: Option<CsvReader>,
    delimiter: u8,
}

impl Group<'_> {
    fn new(g: &ImportFiles, delimiter: u8) -> Group<'_> {
        Group {
            files: g.files.iter(),
            reader: None,
            delimiter,
        }
    }

    /// The next record and where it is (`file line N`).
    fn next(&mut self) -> Result<Option<(Vec<Field>, String)>> {
        loop {
            if let Some(r) = &mut self.reader
                && let Some(rec) = r.next_record()?
            {
                return Ok(Some((rec, format!("{} line {}", r.path().display(), r.record_line()))));
            }
            match self.files.next() {
                Some(f) => self.reader = Some(CsvReader::open(f.clone(), self.delimiter)?),
                None => return Ok(None),
            }
        }
    }
}

fn header(group: &mut Group, g: &ImportFiles, nodes: bool) -> Result<Option<Vec<Column>>> {
    let Some((rec, at)) = group.next()? else {
        return Ok(None);
    };
    let cols = rec.iter().map(|(f, _)| column(f, nodes, &at)).collect::<Result<Vec<_>>>()?;
    let count = |want: fn(&Column) -> bool| cols.iter().filter(|c| want(c)).count();
    if count(|c| matches!(c, Column::Id { .. })) > 1 {
        return Err(Error::Runtime(format!("{at}: the header has more than one :ID column")));
    }
    if !nodes {
        for (what, n) in [
            (":START_ID", count(|c| matches!(c, Column::StartId(_)))),
            (":END_ID", count(|c| matches!(c, Column::EndId(_)))),
        ] {
            if n != 1 {
                return Err(Error::Runtime(format!("{at}: the header needs exactly one {what} column")));
            }
        }
        if g.labels.len() > 1 {
            return Err(Error::Runtime(format!(
                "relationship files {} are given more than one type",
                g.files.first().map(|f| f.display().to_string()).unwrap_or_default()
            )));
        }
    }
    Ok(Some(cols))
}

fn id_key(text: &str, id_type: IdType, at: &str) -> Result<String> {
    match id_type {
        IdType::String => Ok(text.to_string()),
        IdType::Integer => text
            .trim()
            .parse::<i64>()
            .map(|i| i.to_string())
            .map_err(|_| Error::Runtime(format!("{at}: id `{text}` is not an integer"))),
    }
}

/// Reads the inputs into `loader` (nodes first, then relationships, then the indexes).
/// The caller finishes the loader.
pub fn import_csv(loader: &mut BulkLoader, opts: &ImportOptions) -> Result<ImportSummary> {
    let mut sum = ImportSummary::default();
    let mut ids: HashMap<String, HashMap<String, u64>> = HashMap::new();
    let ad = opts.array_delimiter;
    let field_err = |at: &str, col: &str, why: String| Error::Runtime(format!("{at}: field `{col}`: {why}"));

    for g in &opts.nodes {
        let mut group = Group::new(g, opts.delimiter);
        let Some(cols) = header(&mut group, g, true)? else { continue };
        while let Some((rec, at)) = group.next()? {
            let mut labels = g.labels.clone();
            let mut props = BTreeMap::new();
            let mut id: Option<(&str, String)> = None;
            for (col, field) in cols.iter().zip(rec.iter()) {
                match col {
                    Column::Id { space, prop } => {
                        if field.0.is_empty() {
                            continue;
                        }
                        let key = id_key(&field.0, opts.id_type, &at)?;
                        if let Some(p) = prop {
                            let v = match opts.id_type {
                                IdType::String => Value::String(field.0.clone()),
                                IdType::Integer => Value::Int(key.parse().expect("checked by id_key")),
                            };
                            props.insert(p.clone(), v);
                        }
                        id = Some((space, key));
                    }
                    Column::Label => {
                        labels.extend(field.0.split(ad).map(str::trim).filter(|l| !l.is_empty()).map(String::from));
                    }
                    Column::Prop(p) => {
                        if let Some(v) = field_value(field, p, ad).map_err(|e| field_err(&at, &p.name, e))? {
                            props.insert(p.name.clone(), v);
                        }
                    }
                    _ => {}
                }
            }
            if let Some((space, key)) = &id
                && ids.get(*space).is_some_and(|m| m.contains_key(key))
            {
                if opts.skip_duplicate_nodes {
                    sum.skipped_nodes += 1;
                    continue;
                }
                let space = if space.is_empty() { String::new() } else { format!(" in id space `{space}`") };
                return Err(Error::Runtime(format!("{at}: duplicate node id `{key}`{space}")));
            }
            sum.properties += props.len() as u64;
            sum.labels += labels.len() as u64;
            let node = loader.create_node(&labels, props).map_err(|e| at_err(&at, e))?;
            sum.nodes += 1;
            if let Some((space, key)) = id {
                ids.entry(space.to_string()).or_default().insert(key, node.0);
            }
        }
    }

    for g in &opts.relationships {
        let mut group = Group::new(g, opts.delimiter);
        let Some(cols) = header(&mut group, g, false)? else { continue };
        while let Some((rec, at)) = group.next()? {
            let mut rel_type = g.labels.first().cloned().unwrap_or_default();
            let mut props = BTreeMap::new();
            let (mut start, mut end): (Option<u64>, Option<u64>) = (None, None);
            let mut missing: Option<String> = None;
            for (col, field) in cols.iter().zip(rec.iter()) {
                match col {
                    Column::StartId(space) | Column::EndId(space) => {
                        let key = id_key(&field.0, opts.id_type, &at)?;
                        let node = ids.get(space).and_then(|m| m.get(&key)).copied();
                        if node.is_none() && missing.is_none() {
                            let space = if space.is_empty() { String::new() } else { format!(" in id space `{space}`") };
                            missing = Some(format!("node id `{}`{space} is not found", field.0));
                        }
                        if matches!(col, Column::StartId(_)) {
                            start = node;
                        } else {
                            end = node;
                        }
                    }
                    Column::Type => {
                        if !field.0.trim().is_empty() {
                            rel_type = field.0.trim().to_string();
                        }
                    }
                    Column::Prop(p) => {
                        if let Some(v) = field_value(field, p, ad).map_err(|e| field_err(&at, &p.name, e))? {
                            props.insert(p.name.clone(), v);
                        }
                    }
                    _ => {}
                }
            }
            if missing.is_none() && (start.is_none() || end.is_none()) {
                missing = Some("the record has no start or end id".into());
            }
            if missing.is_none() && rel_type.is_empty() {
                missing = Some("the relationship has no type".into());
            }
            if let Some(why) = missing {
                if opts.skip_bad_relationships {
                    sum.skipped_relationships += 1;
                    continue;
                }
                return Err(Error::Runtime(format!("{at}: {why}")));
            }
            let (start, end) = (start.expect("checked"), end.expect("checked"));
            sum.properties += props.len() as u64;
            loader
                .create_relationship(NodeId(start), NodeId(end), &rel_type, props)
                .map_err(|e| at_err(&at, e))?;
            sum.relationships += 1;
        }
    }

    for ix in &opts.indexes {
        loader.create_index(ix.clone())?;
        sum.indexes += 1;
    }
    Ok(sum)
}

fn at_err(at: &str, e: Error) -> Error {
    match e {
        Error::Type(m) => Error::Type(format!("{at}: {m}")),
        Error::Runtime(m) => Error::Runtime(format!("{at}: {m}")),
        other => other,
    }
}
