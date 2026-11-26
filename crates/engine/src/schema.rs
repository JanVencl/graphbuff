//! The shape of a graph, learnt from a sample: the properties of each label and
//! relationship type, with their types, and the label–type–label patterns. Counts are
//! exact; properties and patterns come from up to `sample` nodes per label and
//! relationships per type. Behind `db.schema.nodeTypeProperties()`,
//! `db.schema.relTypeProperties()`, `db.schema.visualization()` and
//! [`crate::Database::schema`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use graphbuff_core::{Direction, GraphStore, NodeId, RelId, Result, Temporal, Value};

use crate::procedures::{ProcedureContext, ProcedureRows};

/// Nodes per label and relationships per type looked at, unless asked otherwise.
pub const DEFAULT_SAMPLE: usize = 1000;
/// Relationships of one sampled node looked at.
const PER_NODE: usize = 100;
/// Relationships scanned in all to find types that the sampled nodes did not reach.
const SCAN_BUDGET: usize = 1_000_000;

/// A graph's schema; see the [module](self).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphSchema {
    /// One per label that some node has, by name.
    pub labels: Vec<EntitySchema>,
    /// One per combination of labels that a sampled node has, as
    /// `db.schema.nodeTypeProperties()` reports them.
    pub node_types: Vec<NodeTypeSchema>,
    /// One per relationship type that some relationship has, by name.
    pub relationship_types: Vec<EntitySchema>,
    /// `(:start)-[:type]->(:end)` seen in the sample, sorted.
    pub patterns: Vec<Pattern>,
}

/// A label or relationship type.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntitySchema {
    pub name: String,
    /// Nodes with the label, or relationships of the type (exact).
    pub count: usize,
    /// How many of them the properties were learnt from.
    pub sampled: usize,
    pub properties: Vec<PropertySchema>,
}

/// The nodes that have exactly these labels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeTypeSchema {
    pub labels: Vec<String>,
    pub properties: Vec<PropertySchema>,
}

/// A property seen in the sample.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PropertySchema {
    pub name: String,
    /// The types of its values, by Neo4j's names (`String`, `Long`, `Double`,
    /// `DoubleArray`, `Date`, …), sorted.
    pub types: Vec<&'static str>,
    /// Whether every sampled node or relationship has it.
    pub mandatory: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pattern {
    pub start: String,
    pub rel_type: String,
    pub end: String,
}

/// The name of the type of a property value, as Neo4j's schema procedures give it.
pub fn property_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Int(_) => "Long",
        Value::Float(_) => "Double",
        Value::String(_) => "String",
        Value::Point(_) => "Point",
        Value::Temporal(t) => match t {
            Temporal::Date(_) => "Date",
            Temporal::LocalTime(_) => "LocalTime",
            Temporal::Time { .. } => "Time",
            Temporal::LocalDateTime { .. } => "LocalDateTime",
            Temporal::DateTime { .. } => "DateTime",
            Temporal::Duration(_) => "Duration",
        },
        Value::List(items) => {
            let mut kinds = items.iter().map(property_type);
            let Some(first) = kinds.next() else { return "List" };
            let numbers = |k: &str| k == "Long" || k == "Double";
            let mut all_same = true;
            let mut all_numbers = numbers(first);
            for k in kinds {
                all_same &= k == first;
                all_numbers &= numbers(k);
            }
            match (all_same, first) {
                (true, "Boolean") => "BooleanArray",
                (true, "Long") => "LongArray",
                (true, "Double") => "DoubleArray",
                (true, "String") => "StringArray",
                (true, "Point") => "PointArray",
                (true, "Date") => "DateArray",
                (true, "LocalTime") => "LocalTimeArray",
                (true, "Time") => "TimeArray",
                (true, "LocalDateTime") => "LocalDateTimeArray",
                (true, "DateTime") => "DateTimeArray",
                (true, "Duration") => "DurationArray",
                _ if all_numbers => "DoubleArray",
                _ => "List",
            }
        }
        Value::Map(_) => "Map",
        Value::Node(_) => "Node",
        Value::Rel(_) => "Relationship",
        Value::Path(_) => "Path",
    }
}

/// Property types and presence over a set of entities.
#[derive(Default)]
struct Props {
    seen: usize,
    keys: BTreeMap<String, (BTreeSet<&'static str>, usize)>,
}

impl Props {
    fn add(&mut self, props: &BTreeMap<String, Value>) {
        self.seen += 1;
        for (k, v) in props {
            let e = self.keys.entry(k.clone()).or_default();
            e.0.insert(property_type(v));
            e.1 += 1;
        }
    }

    fn finish(self) -> Vec<PropertySchema> {
        let seen = self.seen;
        self.keys
            .into_iter()
            .map(|(name, (types, n))| PropertySchema { name, types: types.into_iter().collect(), mandatory: n == seen })
            .collect()
    }
}

/// Labels of nodes, read once each.
struct Labels<'a> {
    store: &'a dyn GraphStore,
    cache: HashMap<NodeId, Vec<String>>,
}

impl Labels<'_> {
    fn of(&mut self, n: NodeId) -> Result<&[String]> {
        if !self.cache.contains_key(&n) {
            let mut l = self.store.node_labels(n)?;
            l.sort();
            self.cache.insert(n, l);
        }
        Ok(&self.cache[&n])
    }
}

/// The schema of `store`, from up to `sample` nodes per label and relationships per
/// type (`0`: all of them). `check` is called now and then, and stops the work when it
/// fails (a timeout, a cancel).
pub(crate) fn sample(store: &dyn GraphStore, sample: usize, check: &dyn Fn() -> Result<()>) -> Result<GraphSchema> {
    let sample = if sample == 0 { usize::MAX } else { sample };
    let mut ticks = 0usize;
    let mut tick = || -> Result<()> {
        ticks += 1;
        if ticks.is_multiple_of(1024) { check() } else { Ok(()) }
    };
    let mut labels = Labels { store, cache: HashMap::new() };

    // Nodes: the first `sample` of each label.
    let mut names: Vec<String> = store.labels().into_iter().filter(|l| store.label_count(l) > 0).collect();
    names.sort();
    names.dedup();
    let mut by_label = Vec::new();
    let mut by_type: BTreeMap<Vec<String>, Props> = BTreeMap::new();
    let mut typed: HashSet<NodeId> = HashSet::new();
    let mut sampled_nodes = Vec::new();
    for name in &names {
        let mut props = Props::default();
        for n in store.nodes_with_label(name).take(sample) {
            let n = n?;
            tick()?;
            let p = store.node_properties(n)?;
            props.add(&p);
            if typed.insert(n) {
                by_type.entry(labels.of(n)?.to_vec()).or_default().add(&p);
                sampled_nodes.push(n);
            }
        }
        by_label.push(EntitySchema {
            name: name.clone(),
            count: store.label_count(name),
            sampled: props.seen,
            properties: props.finish(),
        });
    }

    // Relationships: those of the sampled nodes first, then a scan for the types they
    // did not reach.
    let mut types: Vec<String> =
        store.relationship_types().into_iter().filter(|t| store.relationship_type_count(t) > 0).collect();
    types.sort();
    types.dedup();
    let mut rels: BTreeMap<String, Props> = types.iter().map(|t| (t.clone(), Props::default())).collect();
    let mut seen: HashSet<RelId> = HashSet::new();
    let mut patterns: BTreeSet<Pattern> = BTreeSet::new();
    let mut add = |r: RelId, labels: &mut Labels<'_>, rels: &mut BTreeMap<String, Props>| -> Result<()> {
        let Some(t) = store.relationship_type(r)? else { return Ok(()) };
        let Some(props) = rels.get_mut(&t) else { return Ok(()) };
        if props.seen >= sample || !seen.insert(r) {
            return Ok(());
        }
        props.add(&store.relationship_properties(r)?);
        if let Some((a, b)) = store.relationship_endpoints(r)? {
            let start = labels.of(a)?.to_vec();
            for s in &start {
                for e in labels.of(b)? {
                    patterns.insert(Pattern { start: s.clone(), rel_type: t.clone(), end: e.clone() });
                }
            }
        }
        Ok(())
    };
    for &n in &sampled_nodes {
        for x in store.expand(n, Direction::Outgoing, &[]).take(PER_NODE) {
            tick()?;
            add(x?.rel, &mut labels, &mut rels)?;
        }
    }
    if rels.values().any(|p| p.seen == 0) {
        for r in store.all_relationships().take(SCAN_BUDGET) {
            tick()?;
            add(r?, &mut labels, &mut rels)?;
            if rels.values().all(|p| p.seen >= sample) {
                break;
            }
        }
    }
    check()?;
    let relationship_types = rels
        .into_iter()
        .map(|(name, p)| EntitySchema {
            count: store.relationship_type_count(&name),
            name,
            sampled: p.seen,
            properties: p.finish(),
        })
        .collect();
    Ok(GraphSchema {
        labels: by_label,
        node_types: by_type.into_iter().map(|(labels, p)| NodeTypeSchema { labels, properties: p.finish() }).collect(),
        relationship_types,
        patterns: patterns.into_iter().collect(),
    })
}

/// `` :`A`:`B` `` as Neo4j names a combination of labels, or a relationship type.
fn type_name(names: &[String]) -> String {
    names.iter().map(|n| format!(":`{}`", n.replace('`', "``"))).collect()
}

fn sample_size(name: &str, config: &Value) -> Result<usize> {
    let size = match config {
        Value::Null => return Ok(DEFAULT_SAMPLE),
        Value::Map(m) => {
            if let Some(k) = m.keys().find(|k| *k != "sampleSize") {
                return Err(graphbuff_core::Error::Semantic(format!("{name}: unknown config key `{k}` (known: sampleSize)")));
            }
            m.get("sampleSize")
        }
        o => return Err(graphbuff_core::Error::Type(format!("{name}: config must be a map, not {}", o.type_name()))),
    };
    match size {
        None | Some(Value::Null) => Ok(DEFAULT_SAMPLE),
        Some(Value::Int(n)) if *n == -1 => Ok(0),
        Some(Value::Int(n)) if *n >= 1 => Ok(*n as usize),
        _ => Err(graphbuff_core::Error::Semantic(format!("{name}: sampleSize must be a positive integer, or -1 for all"))),
    }
}

fn of_call(ctx: &ProcedureContext<'_>, name: &str, args: &[Value]) -> Result<GraphSchema> {
    let size = sample_size(name, &args[0])?;
    sample(ctx.store(), size, &|| ctx.check_interrupt())
}

fn property_rows(props: &[PropertySchema], mut row: impl FnMut(Value, Value, Value)) {
    if props.is_empty() {
        row(Value::Null, Value::Null, Value::Bool(false));
    }
    for p in props {
        let types = Value::List(p.types.iter().map(|t| Value::String((*t).into())).collect());
        row(Value::String(p.name.clone()), types, Value::Bool(p.mandatory));
    }
}

/// `db.schema.nodeTypeProperties`: nodeType, nodeLabels, propertyName, propertyTypes,
/// mandatory.
pub(crate) fn node_type_properties(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let s = of_call(ctx, "db.schema.nodeTypeProperties", args)?;
    let mut rows = Vec::new();
    for t in s.node_types {
        let name = Value::String(type_name(&t.labels));
        let labels = Value::List(t.labels.iter().cloned().map(Value::String).collect());
        property_rows(&t.properties, |p, ty, m| rows.push(vec![name.clone(), labels.clone(), p, ty, m]));
    }
    Ok(rows)
}

/// `db.schema.relTypeProperties`: relType, propertyName, propertyTypes, mandatory.
pub(crate) fn rel_type_properties(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let s = of_call(ctx, "db.schema.relTypeProperties", args)?;
    let mut rows = Vec::new();
    for t in s.relationship_types {
        let name = Value::String(type_name(std::slice::from_ref(&t.name)));
        property_rows(&t.properties, |p, ty, m| rows.push(vec![name.clone(), p, ty, m]));
    }
    Ok(rows)
}

/// `db.schema.visualization`: one row of `nodes` (maps with the label's `name` and
/// `count`) and `relationships` (maps with `type`, `start` and `end` labels).
pub(crate) fn visualization(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let s = of_call(ctx, "db.schema.visualization", args)?;
    let map = |items: Vec<(&str, Value)>| Value::Map(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    let nodes = s
        .labels
        .into_iter()
        .map(|l| map(vec![("name", Value::String(l.name)), ("count", Value::Int(l.count as i64))]))
        .collect();
    let rels = s
        .patterns
        .into_iter()
        .map(|p| map(vec![("type", Value::String(p.rel_type)), ("start", Value::String(p.start)), ("end", Value::String(p.end))]))
        .collect();
    Ok(vec![vec![Value::List(nodes), Value::List(rels)]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(property_type(&Value::List(vec![Value::Int(1), Value::Float(2.0)])), "DoubleArray");
        assert_eq!(property_type(&Value::List(vec![Value::Int(1), Value::Int(2)])), "LongArray");
        assert_eq!(property_type(&Value::List(vec![Value::Int(1), Value::String("a".into())])), "List");
        assert_eq!(property_type(&Value::List(vec![])), "List");
        assert_eq!(type_name(&["A".into(), "b`c".into()]), ":`A`:`b``c`");
    }
}
