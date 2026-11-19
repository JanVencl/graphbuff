//! Writing results: as a table, tab-separated, CSV or the server's JSON.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::Duration;

use graphbuff::json::Json;

use crate::client::{Outcome, compact};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    /// A boxed table with a summary line.
    Table,
    /// A header line and one line per row, tab-separated.
    Plain,
    /// RFC 4180 CSV with a header line.
    Csv,
    /// The server's JSON, one result per line.
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Result<Format, String> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Ok(Format::Table),
            "plain" | "tsv" => Ok(Format::Plain),
            "csv" => Ok(Format::Csv),
            "json" => Ok(Format::Json),
            _ => Err(format!("unknown format `{s}`; use table, plain, csv or json")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Table => "table",
            Format::Plain => "plain",
            Format::Csv => "csv",
            Format::Json => "json",
        }
    }
}

/// Writes `r` in `format`; `elapsed` goes into the table's summary.
pub fn write(out: &mut impl Write, format: Format, r: &Outcome, elapsed: Duration) -> io::Result<()> {
    match format {
        Format::Json => writeln!(out, "{}", r.raw.trim()),
        Format::Plain => {
            if !r.columns.is_empty() {
                writeln!(out, "{}", r.columns.iter().map(|c| tsv(c)).collect::<Vec<_>>().join("\t"))?;
            }
            for row in &r.rows {
                let cells: Vec<String> = row.iter().map(|v| tsv(&text(v))).collect();
                writeln!(out, "{}", cells.join("\t"))?;
            }
            Ok(())
        }
        Format::Csv => {
            if !r.columns.is_empty() {
                writeln!(out, "{}", r.columns.iter().map(|c| csv(c)).collect::<Vec<_>>().join(","))?;
            }
            for row in &r.rows {
                let cells: Vec<String> = row.iter().map(|v| csv(&text(v))).collect();
                writeln!(out, "{}", cells.join(","))?;
            }
            Ok(())
        }
        Format::Table => {
            if let Some(plan) = &r.plan {
                write_plan(out, plan)?;
            }
            if !r.columns.is_empty() {
                let cells: Vec<Vec<String>> =
                    r.rows.iter().map(|row| row.iter().map(|v| one_line(&cypher(v))).collect()).collect();
                write_table(out, &r.columns, &cells)?;
            }
            writeln!(out, "{}", summary(r, elapsed))
        }
    }
}

/// `3 rows, 2 ms` and what the statement changed.
pub fn summary(r: &Outcome, elapsed: Duration) -> String {
    let mut s = match r.rows.len() {
        1 => "1 row".to_string(),
        n => format!("{n} rows"),
    };
    let ms = elapsed.as_secs_f64() * 1e3;
    let _ = write!(s, ", {}", if ms < 10.0 { format!("{ms:.1} ms") } else { format!("{ms:.0} ms") });
    let changes = changes(&r.stats);
    if !changes.is_empty() {
        let _ = write!(s, "\n{changes}");
    }
    s
}

/// The counters of `stats` that are not zero: `Nodes created: 1, Properties set: 2`.
pub fn changes(stats: &BTreeMap<String, Json>) -> String {
    const NAMES: &[(&str, &str)] = &[
        ("nodesCreated", "Nodes created"),
        ("nodesDeleted", "Nodes deleted"),
        ("relationshipsCreated", "Relationships created"),
        ("relationshipsDeleted", "Relationships deleted"),
        ("propertiesSet", "Properties set"),
        ("labelsAdded", "Labels added"),
        ("labelsRemoved", "Labels removed"),
        ("indexesAdded", "Indexes added"),
        ("indexesRemoved", "Indexes removed"),
        ("constraintsAdded", "Constraints added"),
        ("constraintsRemoved", "Constraints removed"),
        ("systemUpdates", "System updates"),
    ];
    NAMES
        .iter()
        .filter_map(|(key, name)| match stats.get(*key) {
            Some(Json::Int(n)) if *n != 0 => Some(format!("{name}: {n}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_table(out: &mut impl Write, columns: &[String], cells: &[Vec<String>]) -> io::Result<()> {
    let mut widths: Vec<usize> = columns.iter().map(|c| width(c)).collect();
    for row in cells {
        for (i, c) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(width(c));
            }
        }
    }
    let rule = {
        let mut s = String::from("+");
        for w in &widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s
    };
    let line = |out: &mut dyn Write, values: &[String]| -> io::Result<()> {
        let mut s = String::from("|");
        for (i, w) in widths.iter().enumerate() {
            let v = values.get(i).map_or("", String::as_str);
            let _ = write!(s, " {v}{} |", " ".repeat(w - width(v)));
        }
        writeln!(out, "{s}")
    };
    writeln!(out, "{rule}")?;
    line(out, columns)?;
    writeln!(out, "{rule}")?;
    for row in cells {
        line(out, row)?;
    }
    if !cells.is_empty() {
        writeln!(out, "{rule}")?;
    }
    Ok(())
}

fn write_plan(out: &mut impl Write, plan: &Json) -> io::Result<()> {
    let Json::Object(p) = plan else { return Ok(()) };
    let profiled = matches!(p.get("profiled"), Some(Json::Bool(true)));
    let Some(Json::Array(lines)) = p.get("lines") else { return Ok(()) };
    let mut columns = vec!["Operator".to_string(), "Estimated rows".to_string()];
    if profiled {
        columns.extend(["Rows".to_string(), "Time (ms)".to_string()]);
    }
    let rows: Vec<Vec<String>> = lines
        .iter()
        .filter_map(|l| match l {
            Json::Object(l) => Some(l),
            _ => None,
        })
        .map(|l| {
            let depth = match l.get("depth") {
                Some(Json::Int(d)) => (*d).max(0) as usize,
                _ => 0,
            };
            let op = match l.get("operator") {
                Some(Json::String(s)) => s.as_str(),
                _ => "?",
            };
            let num = |k: &str| match l.get(k) {
                Some(Json::Float(f)) => format!("{f:.1}"),
                Some(v) => text(v),
                None => String::new(),
            };
            let mut row = vec![one_line(&format!("{}{op}", "  ".repeat(depth))), num("estimatedRows")];
            if profiled {
                row.extend([num("rows"), num("timeMs")]);
            }
            row
        })
        .collect();
    write_table(out, &columns, &rows)
}

/// The display width of `s`, counting characters.
fn width(s: &str) -> usize {
    s.chars().count()
}

fn one_line(s: &str) -> String {
    s.replace('\r', "\\r").replace('\n', "\\n").replace('\t', "\\t")
}

/// A value for plain and CSV output: strings as they are, everything else as Cypher.
pub fn text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => cypher(other),
    }
}

fn tsv(s: &str) -> String {
    one_line(s)
}

fn csv(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// A value written the way Cypher writes it: `"text"`, `[1, 2]`, `{a: 1}`,
/// `(:Person {name: "Ann"})`, `[:KNOWS]`, `(:A)-[:R]->(:B)`.
pub fn cypher(v: &Json) -> String {
    let mut s = String::new();
    write_cypher(&mut s, v);
    s
}

fn write_cypher(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Float(f) => {
            let _ = write!(out, "{f:?}");
        }
        Json::String(s) => {
            let _ = write!(out, "{s:?}");
        }
        Json::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_cypher(out, x);
            }
            out.push(']');
        }
        Json::Object(m) => match m.get("$type") {
            Some(Json::String(t)) if t == "node" => write_node(out, m),
            Some(Json::String(t)) if t == "relationship" => {
                out.push('[');
                write_rel_inner(out, m);
                out.push(']');
            }
            Some(Json::String(t)) if t == "path" => write_path(out, m),
            Some(Json::String(t)) if t == "point" => write_point(out, m),
            _ => write_map(out, m),
        },
    }
}

fn write_map(out: &mut String, m: &BTreeMap<String, Json>) {
    out.push('{');
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_name(out, k);
        out.push_str(": ");
        write_cypher(out, v);
    }
    out.push('}');
}

/// A label, type or key, in backticks unless it is a plain name.
fn write_name(out: &mut String, name: &str) {
    let plain = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if plain {
        out.push_str(name);
    } else {
        let _ = write!(out, "`{}`", name.replace('`', "``"));
    }
}

fn write_props(out: &mut String, m: &BTreeMap<String, Json>) {
    if let Some(Json::Object(p)) = m.get("properties")
        && !p.is_empty()
    {
        out.push(' ');
        write_map(out, p);
    }
}

fn write_node(out: &mut String, m: &BTreeMap<String, Json>) {
    out.push('(');
    if let Some(Json::Array(labels)) = m.get("labels") {
        for l in labels {
            out.push(':');
            match l {
                Json::String(l) => write_name(out, l),
                other => out.push_str(&compact(other)),
            }
        }
    }
    let before = out.len();
    write_props(out, m);
    // `( {a: 1})` reads oddly: drop the space when there is no label.
    if out.as_bytes().get(before.wrapping_sub(1)) == Some(&b'(') && out.len() > before {
        out.remove(before);
    }
    out.push(')');
}

fn write_rel_inner(out: &mut String, m: &BTreeMap<String, Json>) {
    if let Some(Json::String(t)) = m.get("type") {
        out.push(':');
        write_name(out, t);
    }
    write_props(out, m);
}

fn write_path(out: &mut String, m: &BTreeMap<String, Json>) {
    let empty = Vec::new();
    let list = |k: &str| match m.get(k) {
        Some(Json::Array(a)) => a,
        _ => &empty,
    };
    let (nodes, rels) = (list("nodes"), list("relationships"));
    let id = |v: &Json, k: &str| match v {
        Json::Object(o) => o.get(k).cloned(),
        _ => None,
    };
    for (i, node) in nodes.iter().enumerate() {
        if i > 0
            && let Some(rel @ Json::Object(r)) = rels.get(i - 1)
        {
            let forward = id(rel, "start") == id(&nodes[i - 1], "id");
            out.push_str(if forward { "-[" } else { "<-[" });
            write_rel_inner(out, r);
            out.push_str(if forward { "]->" } else { "]-" });
        }
        write_cypher(out, node);
    }
}

fn write_point(out: &mut String, m: &BTreeMap<String, Json>) {
    out.push_str("point({");
    let mut first = true;
    for k in ["srid", "x", "y", "z"] {
        if let Some(v) = m.get(k)
            && *v != Json::Null
        {
            if !first {
                out.push_str(", ");
            }
            first = false;
            let _ = write!(out, "{k}: ");
            write_cypher(out, v);
        }
    }
    out.push_str("})");
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphbuff::json::parse;

    fn c(s: &str) -> String {
        cypher(&parse(s).unwrap())
    }

    #[test]
    fn values_as_cypher() {
        assert_eq!(c(r#"[1, 2.0, "a\"b", null, true]"#), r#"[1, 2.0, "a\"b", null, true]"#);
        assert_eq!(c(r#"{"a": 1, "b c": "x"}"#), r#"{a: 1, `b c`: "x"}"#);
        assert_eq!(
            c(r#"{"$type":"node","id":0,"labels":["Person"],"properties":{"name":"Ann"}}"#),
            r#"(:Person {name: "Ann"})"#
        );
        assert_eq!(c(r#"{"$type":"node","id":0,"labels":[],"properties":{"a":1}}"#), "({a: 1})");
        assert_eq!(c(r#"{"$type":"node","id":0,"labels":[],"properties":{}}"#), "()");
        let path = r#"{"$type":"path",
            "nodes":[{"$type":"node","id":0,"labels":["A"],"properties":{}},
                     {"$type":"node","id":1,"labels":["B"],"properties":{}},
                     {"$type":"node","id":2,"labels":["C"],"properties":{}}],
            "relationships":[{"$type":"relationship","id":0,"type":"R","start":0,"end":1,"properties":{}},
                             {"$type":"relationship","id":1,"type":"S","start":2,"end":1,"properties":{"w":1}}]}"#;
        assert_eq!(c(path), "(:A)-[:R]->(:B)<-[:S {w: 1}]-(:C)");
        assert_eq!(
            c(r#"{"$type":"point","crs":"cartesian","srid":7203,"x":1.0,"y":2.0}"#),
            "point({srid: 7203, x: 1.0, y: 2.0})"
        );
    }

    #[test]
    fn table() {
        let r = Outcome {
            columns: vec!["n".into(), "name".into()],
            rows: vec![vec![Json::Int(1), Json::String("Ann".into())]],
            ..Outcome::default()
        };
        let mut out = Vec::new();
        write(&mut out, Format::Table, &r, Duration::from_millis(12)).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "+---+-------+\n| n | name  |\n+---+-------+\n| 1 | \"Ann\" |\n+---+-------+\n1 row, 12 ms\n"
        );
    }

    #[test]
    fn csv_quoting() {
        assert_eq!(csv("a,b"), "\"a,b\"");
        assert_eq!(csv("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv("plain"), "plain");
    }
}
