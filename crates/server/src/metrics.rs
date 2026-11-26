//! `GET /metrics`: server and database metrics in the Prometheus text format.
//!
//! Counters (queries by protocol and outcome, a query latency histogram, connections,
//! failed logins and lockouts) are kept here as the server runs; gauges (open
//! transactions and connections, database sizes, memory) are read when scraped.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use graphbuff::{Error, StorageKind};

use crate::api::Ctx;

/// The protocols a server speaks, in the order metrics list them.
const PROTOCOLS: [&str; 3] = ["http", "bolt", "mcp"];

/// Upper bounds of the query latency histogram's buckets, in seconds.
const BUCKETS: [f64; 12] = [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// The outcomes a query is counted under.
const OUTCOMES: [&str; 5] = ["ok", "error", "timeout", "cancelled", "memory_limit"];

/// The outcomes an MCP tool call is counted under: `error` is a result the model is told
/// is one, `limited` a call refused by the rate or concurrency limit.
const TOOL_OUTCOMES: [&str; 3] = ["ok", "error", "limited"];

#[derive(Default)]
struct Histogram {
    /// Per bucket of [`BUCKETS`], the queries that took at most its bound (not
    /// cumulative; summed when shown).
    buckets: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

impl Histogram {
    fn add(&mut self, elapsed: Duration) {
        let secs = elapsed.as_secs_f64();
        self.count += 1;
        self.sum += secs;
        if let Some(i) = BUCKETS.iter().position(|b| secs <= *b) {
            self.buckets[i] += 1;
        }
    }

    /// Writes the samples of histogram `name` with the label `labels` (`key="value"`).
    fn write(&self, out: &mut String, name: &str, labels: &str) {
        let mut cumulative = 0;
        for (b, n) in BUCKETS.iter().zip(self.buckets) {
            cumulative += n;
            let _ = writeln!(out, "{name}_bucket{{{labels},le=\"{b}\"}} {cumulative}");
        }
        let _ = writeln!(out, "{name}_bucket{{{labels},le=\"+Inf\"}} {}", self.count);
        let _ = writeln!(out, "{name}_sum{{{labels}}} {}", self.sum);
        let _ = writeln!(out, "{name}_count{{{labels}}} {}", self.count);
    }
}

#[derive(Default)]
struct State {
    /// Per (protocol, outcome).
    queries: BTreeMap<(&'static str, &'static str), u64>,
    latency: BTreeMap<&'static str, Histogram>,
    failed_logins: BTreeMap<&'static str, u64>,
    /// Per protocol: connections accepted, and open now.
    accepted: BTreeMap<&'static str, u64>,
    open: BTreeMap<&'static str, u64>,
    /// MCP tool calls per (tool, outcome), and their durations per tool.
    tool_calls: BTreeMap<(&'static str, &'static str), u64>,
    tool_latency: BTreeMap<&'static str, Histogram>,
}

/// The server's counters.
pub(crate) struct Metrics {
    started: Instant,
    state: Mutex<State>,
    lockouts: AtomicU64,
    refused: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            started: Instant::now(),
            state: Mutex::default(),
            lockouts: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }
    }
}

/// `protocol` as one of [`PROTOCOLS`] (a `'static` key).
fn protocol_key(protocol: &str) -> &'static str {
    PROTOCOLS.into_iter().find(|p| *p == protocol).unwrap_or("other")
}

fn outcome(error: Option<&Error>) -> &'static str {
    match error {
        None => "ok",
        Some(Error::Timeout(_)) => "timeout",
        Some(Error::Cancelled(_)) => "cancelled",
        Some(Error::MemoryLimit(_)) => "memory_limit",
        Some(_) => "error",
    }
}

impl Metrics {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts a finished query.
    pub fn query(&self, protocol: &str, elapsed: Duration, error: Option<&Error>) {
        let protocol = protocol_key(protocol);
        let mut st = self.state();
        *st.queries.entry((protocol, outcome(error))).or_insert(0) += 1;
        st.latency.entry(protocol).or_default().add(elapsed);
    }

    /// Counts a finished MCP tool call.
    pub fn mcp_call(&self, tool: &'static str, outcome: &'static str, elapsed: Duration) {
        let mut st = self.state();
        *st.tool_calls.entry((tool, outcome)).or_insert(0) += 1;
        st.tool_latency.entry(tool).or_default().add(elapsed);
    }

    pub fn failed_login(&self, protocol: &str) {
        *self.state().failed_logins.entry(protocol_key(protocol)).or_insert(0) += 1;
    }

    /// Counts a client locked out by failed logins.
    pub fn lockout(&self) {
        self.lockouts.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a connection refused because too many were open.
    pub fn refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_opened(&self, protocol: &str) {
        let protocol = protocol_key(protocol);
        let mut st = self.state();
        *st.accepted.entry(protocol).or_insert(0) += 1;
        *st.open.entry(protocol).or_insert(0) += 1;
    }

    pub fn connection_closed(&self, protocol: &str) {
        if let Some(n) = self.state().open.get_mut(protocol_key(protocol)) {
            *n = n.saturating_sub(1);
        }
    }
}

/// `value` as a label value: quoted, with `\`, `"` and newlines escaped.
fn label(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Writes a metric family's `HELP` and `TYPE` lines.
fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// The metrics of `ctx`'s server, in the Prometheus text format (version 0.0.4).
pub(crate) fn render(ctx: &Ctx) -> String {
    let m = &ctx.metrics;
    let mut out = String::new();
    family(&mut out, "graphbuff_info", "gauge", "The server's version.");
    let _ = writeln!(out, "graphbuff_info{{version={}}} 1", label(env!("CARGO_PKG_VERSION")));
    family(&mut out, "graphbuff_uptime_seconds", "gauge", "Seconds since the server started.");
    let _ = writeln!(out, "graphbuff_uptime_seconds {:.3}", m.started.elapsed().as_secs_f64());

    {
        let st = m.state();
        family(&mut out, "graphbuff_queries_total", "counter", "Queries run, by protocol and outcome.");
        for p in PROTOCOLS {
            for o in OUTCOMES {
                let n = st.queries.get(&(p, o)).copied().unwrap_or(0);
                let _ = writeln!(out, "graphbuff_queries_total{{protocol=\"{p}\",outcome=\"{o}\"}} {n}");
            }
        }
        family(&mut out, "graphbuff_query_duration_seconds", "histogram", "How long queries ran, by protocol.");
        let empty = Histogram::default();
        for p in PROTOCOLS {
            st.latency.get(p).unwrap_or(&empty).write(&mut out, "graphbuff_query_duration_seconds", &format!("protocol=\"{p}\""));
        }
        family(&mut out, "graphbuff_connections_accepted_total", "counter", "Connections accepted, by protocol.");
        for p in PROTOCOLS {
            let _ = writeln!(out, "graphbuff_connections_accepted_total{{protocol=\"{p}\"}} {}", st.accepted.get(p).unwrap_or(&0));
        }
        family(&mut out, "graphbuff_connections", "gauge", "Connections open now, by protocol.");
        for p in PROTOCOLS {
            let _ = writeln!(out, "graphbuff_connections{{protocol=\"{p}\"}} {}", st.open.get(p).unwrap_or(&0));
        }
        family(&mut out, "graphbuff_failed_logins_total", "counter", "Logins refused for wrong or missing credentials, by protocol.");
        for p in PROTOCOLS {
            let _ = writeln!(out, "graphbuff_failed_logins_total{{protocol=\"{p}\"}} {}", st.failed_logins.get(p).unwrap_or(&0));
        }
        family(&mut out, "graphbuff_mcp_tool_calls_total", "counter", "MCP tool calls, by tool and outcome.");
        for t in crate::mcp::tool_names() {
            for o in TOOL_OUTCOMES {
                let n = st.tool_calls.get(&(t, o)).unwrap_or(&0);
                let _ = writeln!(out, "graphbuff_mcp_tool_calls_total{{tool=\"{t}\",outcome=\"{o}\"}} {n}");
            }
        }
        family(&mut out, "graphbuff_mcp_tool_call_duration_seconds", "histogram", "How long MCP tool calls took, by tool.");
        for t in crate::mcp::tool_names() {
            st.tool_latency.get(t).unwrap_or(&empty).write(&mut out, "graphbuff_mcp_tool_call_duration_seconds", &format!("tool=\"{t}\""));
        }
    }
    family(&mut out, "graphbuff_connections_refused_total", "counter", "Connections refused because too many were open.");
    let _ = writeln!(out, "graphbuff_connections_refused_total {}", m.refused.load(Ordering::Relaxed));
    family(&mut out, "graphbuff_login_lockouts_total", "counter", "Clients locked out after too many failed logins.");
    let _ = writeln!(out, "graphbuff_login_lockouts_total {}", m.lockouts.load(Ordering::Relaxed));

    family(&mut out, "graphbuff_transactions", "gauge", "Open transactions (and queries run on their own), by protocol and status.");
    let mut tx: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for r in ctx.running.rows() {
        *tx.entry((r.protocol, r.status)).or_insert(0) += 1;
    }
    for p in PROTOCOLS {
        for s in ["Running", "Idle"] {
            let n = tx.get(&(p, s)).unwrap_or(&0);
            let _ = writeln!(out, "graphbuff_transactions{{protocol=\"{p}\",status=\"{}\"}} {n}", s.to_lowercase());
        }
    }

    let dbs = ctx.mgr.databases();
    family(&mut out, "graphbuff_database_nodes", "gauge", "Nodes, by database.");
    for d in &dbs {
        let _ = writeln!(out, "graphbuff_database_nodes{{database={}}} {}", label(&d.name), d.nodes);
    }
    family(&mut out, "graphbuff_database_relationships", "gauge", "Relationships, by database.");
    for d in &dbs {
        let _ = writeln!(out, "graphbuff_database_relationships{{database={}}} {}", label(&d.name), d.relationships);
    }
    family(&mut out, "graphbuff_database_store_bytes", "gauge", "Bytes of the files a durable database keeps on disk.");
    for d in &dbs {
        if !matches!(d.storage, StorageKind::InMemory)
            && let Some(bytes) = ctx.mgr.disk_bytes(&d.name)
        {
            let _ = writeln!(out, "graphbuff_database_store_bytes{{database={}}} {bytes}", label(&d.name));
        }
    }
    family(&mut out, "graphbuff_database_cached_indexes", "gauge", "Vector and full-text indexes whose contents are cached in memory, by database.");
    for d in &dbs {
        if let Ok(db) = ctx.mgr.database(&d.name) {
            let _ = writeln!(out, "graphbuff_database_cached_indexes{{database={}}} {}", label(&d.name), db.cached_indexes());
        }
    }
    if let Some(bytes) = graphbuff::accounting::total_memory() {
        family(&mut out, "graphbuff_memory_bytes", "gauge", "Memory the process has allocated.");
        let _ = writeln!(out, "graphbuff_memory_bytes {bytes}");
    }
    if let Some(e) = ctx.mgr.embedder() {
        embedding(&mut out, e.as_ref());
    }
    out
}

/// The `graphbuff_embed_*` metrics, when embedding models are set up.
fn embedding(out: &mut String, e: &dyn graphbuff::Embedder) {
    let s = e.stats();
    for (name, kind, help, value) in [
        ("graphbuff_embed_calls_total", "counter", "Embedding calls (vector.embed rows and vector.embedBatch calls) that succeeded.", s.calls as f64),
        ("graphbuff_embed_texts_total", "counter", "Texts embedded.", s.texts as f64),
        ("graphbuff_embed_tokens_total", "counter", "Tokens embedded (padding not counted).", s.tokens as f64),
        ("graphbuff_embed_seconds_total", "counter", "Time spent embedding.", s.seconds),
        ("graphbuff_embed_model_loads_total", "counter", "Embedding models loaded into memory.", s.model_loads as f64),
        ("graphbuff_embed_model_load_seconds_total", "counter", "Time spent loading embedding models.", s.load_seconds),
    ] {
        family(out, name, kind, help);
        let _ = writeln!(out, "{name} {value}");
    }
    family(out, "graphbuff_embed_model_loaded", "gauge", "Whether an installed embedding model is in memory now, by model.");
    for m in e.models() {
        let _ = writeln!(out, "graphbuff_embed_model_loaded{{model={}}} {}", label(&m.name), u8::from(m.loaded));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_buckets() {
        let m = Metrics::default();
        m.query("http", Duration::from_millis(3), None);
        m.query("http", Duration::from_secs(20), Some(&Error::Timeout("t".into())));
        m.query("bolt", Duration::from_micros(10), Some(&Error::Semantic("x".into())));
        let st = m.state();
        assert_eq!(st.queries[&("http", "ok")], 1);
        assert_eq!(st.queries[&("http", "timeout")], 1);
        assert_eq!(st.queries[&("bolt", "error")], 1);
        let h = &st.latency["http"];
        assert_eq!((h.count, h.buckets[1], h.buckets.iter().sum::<u64>()), (2, 1, 1));
        assert_eq!(label("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }
}
