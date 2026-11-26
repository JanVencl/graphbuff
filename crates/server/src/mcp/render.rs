//! Query results for a model: compact JSON text, capped in rows and bytes, with long
//! vectors and strings shortened.

use graphbuff::{Error, PlanDescription, QueryStats, ResultValue, RowSink};

use crate::json::{self, Json};

use super::rpc::{encode, string};

/// Lists of more numbers than this are shown as `"<vector N>"`.
const MAX_NUMBERS: usize = 16;
/// Strings longer than this (in bytes) are cut.
const MAX_STRING: usize = 4096;

/// Collects rows up to `max_rows` and `max_bytes` (of their JSON).
pub(crate) struct Capped {
    columns: Vec<String>,
    rows: Vec<Json>,
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
    /// Rows the query produced (counted on past the cap unless it stops the query).
    total: usize,
    /// A row did not fit.
    pub truncated: bool,
    /// Stop the query at the first row that does not fit (reads only: a write stopped
    /// half way would be rolled back).
    stop_when_full: bool,
    /// The query was stopped by this sink, not by an error of its own.
    pub stopped: bool,
}

impl Capped {
    pub(crate) fn new(max_rows: usize, max_bytes: usize, stop_when_full: bool) -> Capped {
        Capped {
            columns: Vec::new(),
            rows: Vec::new(),
            bytes: 0,
            max_rows,
            max_bytes,
            total: 0,
            truncated: false,
            stop_when_full,
            stopped: false,
        }
    }

    /// Takes the rows of a result that was not streamed.
    pub(crate) fn feed(&mut self, columns: &[String], rows: Vec<Vec<ResultValue>>) {
        let _ = RowSink::columns(self, columns);
        for r in rows {
            if self.row(r).is_err() {
                break;
            }
        }
    }

    /// The text for the model: `{"columns","rows"}`, with `"truncated"` and a hint when
    /// rows were left out, the update counts that are not zero, and the plan of `EXPLAIN`
    /// and `PROFILE`. `extra` fields come first.
    pub(crate) fn render(self, stats: Option<&QueryStats>, plan: Option<&PlanDescription>, extra: Vec<(&str, Json)>) -> String {
        let mut out: Vec<(&str, Json)> = extra;
        out.push(("columns", Json::Array(self.columns.into_iter().map(Json::String).collect())));
        let shown = self.rows.len();
        out.push(("rows", Json::Array(self.rows)));
        if self.truncated {
            out.push(("truncated", Json::Bool(true)));
            out.push(("shown", Json::Int(shown as i64)));
            if !self.stopped {
                out.push(("rowCount", Json::Int(self.total as i64)));
            }
            out.push((
                "hint",
                string(format!(
                    "only the first {shown} rows fit (at most {} rows, {} KiB); use LIMIT, aggregate with count(*) \
                     or collect(), or return fewer properties",
                    self.max_rows,
                    self.max_bytes / 1024
                )),
            ));
        }
        let (stats, plan) = stats_and_plan(stats, plan);
        if let Some(s) = stats {
            out.push(("stats", s));
        }
        if let Some(p) = plan {
            out.push(("plan", p));
        }
        // Keys in this order, not sorted: rows before the hint and the plan.
        let mut text = String::from("{");
        for (i, (k, v)) in out.iter().enumerate() {
            if i > 0 {
                text.push(',');
            }
            json::write_str(&mut text, k);
            text.push(':');
            super::rpc::write(&mut text, v);
        }
        text.push('}');
        text
    }
}

impl RowSink for Capped {
    fn columns(&mut self, columns: &[String]) -> graphbuff::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn row(&mut self, row: Vec<ResultValue>) -> graphbuff::Result<()> {
        self.total += 1;
        if !self.truncated {
            let mut text = String::new();
            json::write_row(&mut text, &row);
            let value = compact(json::parse(&text).unwrap_or(Json::Null));
            let len = encode(&value).len() + 1;
            if self.rows.len() >= self.max_rows || self.bytes + len > self.max_bytes {
                self.truncated = true;
            } else {
                self.bytes += len;
                self.rows.push(value);
            }
        }
        if self.truncated && self.stop_when_full {
            self.stopped = true;
            return Err(Error::Cancelled("the result reached its size limit".into()));
        }
        Ok(())
    }
}

/// `v` with long number lists replaced by `"<vector N>"` and long strings cut.
pub(crate) fn compact(v: Json) -> Json {
    match v {
        Json::Array(items) if items.len() > MAX_NUMBERS && items.iter().all(|x| matches!(x, Json::Int(_) | Json::Float(_))) => {
            string(format!("<vector {}>", items.len()))
        }
        Json::Array(items) => Json::Array(items.into_iter().map(compact).collect()),
        Json::Object(m) => Json::Object(m.into_iter().map(|(k, v)| (k, compact(v))).collect()),
        Json::String(s) if s.len() > MAX_STRING => {
            let mut end = MAX_STRING;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            string(format!("{}…[cut: {} bytes in all]", &s[..end], s.len()))
        }
        other => other,
    }
}

/// The update counts that are not zero, and the plan, as JSON.
fn stats_and_plan(stats: Option<&QueryStats>, plan: Option<&PlanDescription>) -> (Option<Json>, Option<Json>) {
    let mut text = String::new();
    json::write_result_head(&mut text, &[]);
    json::write_result_tail(&mut text, &stats.cloned().unwrap_or_default(), plan);
    let Ok(Json::Object(mut m)) = json::parse(&text) else { return (None, None) };
    let stats = match m.remove("stats") {
        Some(Json::Object(s)) => {
            let nonzero: std::collections::BTreeMap<String, Json> =
                s.into_iter().filter(|(k, v)| k != "containsUpdates" && !matches!(v, Json::Int(0))).collect();
            (!nonzero.is_empty()).then_some(Json::Object(nonzero))
        }
        _ => None,
    };
    (stats, m.remove("plan"))
}

/// An error for the model: its code and message.
pub(crate) fn error_text(e: &Error) -> String {
    let mut text = e.to_string();
    if let Error::Syntax { .. } = e {
        text.push_str(
            "\nGraphBuff supports most of Cypher 5. Not supported: quantified path patterns ((a)-[:R]->{1,3}(b): \
             write (a)-[:R*1..3]->(b)) and SHORTEST k / ALL SHORTEST / ANY selectors (use shortestPath() or \
             allShortestPaths()).",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_compaction() {
        let row = |i: i64| vec![ResultValue::Int(i), ResultValue::String("x".into())];
        let mut c = Capped::new(2, 1 << 20, true);
        RowSink::columns(&mut c, &["n".into(), "s".into()]).unwrap();
        c.row(row(1)).unwrap();
        c.row(row(2)).unwrap();
        assert!(c.row(row(3)).is_err());
        assert!(c.stopped && c.truncated);
        let text = c.render(None, None, Vec::new());
        assert!(text.starts_with(r#"{"columns":["n","s"],"rows":[[1,"x"],[2,"x"]],"truncated":true,"shown":2,"hint":"#), "{text}");
        assert!(!text.contains("rowCount"));

        // A write is not stopped: the rows are counted on.
        let mut c = Capped::new(1, 1 << 20, false);
        c.feed(&["n".into()], (0..5).map(|i| vec![ResultValue::Int(i)]).collect());
        let stats = QueryStats { nodes_created: 5, ..QueryStats::default() };
        let text = c.render(Some(&stats), None, vec![("dryRun", Json::Bool(true))]);
        assert!(text.starts_with(r#"{"dryRun":true,"columns":["n"],"rows":[[0]],"truncated":true,"shown":1,"rowCount":5,"#), "{text}");
        assert!(text.ends_with(r#""stats":{"nodesCreated":5}}"#), "{text}");

        // Bytes.
        let mut c = Capped::new(100, 20, true);
        c.feed(&["s".into()], (0..5).map(|_| vec![ResultValue::String("abcdefgh".into())]).collect());
        assert!(c.render(None, None, Vec::new()).contains(r#""shown":1"#));

        let v = compact(json::parse(&format!("[[{}],[1,2],\"{}\"]", vec!["0.5"; 384].join(","), "é".repeat(3000))).unwrap());
        let Json::Array(a) = v else { panic!() };
        assert_eq!(a[0], string("<vector 384>"));
        assert_eq!(a[1], json::parse("[1,2]").unwrap());
        let Json::String(s) = &a[2] else { panic!() };
        assert!(s.ends_with("…[cut: 6000 bytes in all]") && s.len() < 4200, "{}", s.len());
    }
}
