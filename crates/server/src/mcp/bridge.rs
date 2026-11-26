//! `graphbuff-server mcp`: MCP over standard input and output, for clients that start
//! their MCP servers as programs (Claude Desktop, and others). Each message read is sent
//! to a running server's MCP endpoint, and its answer written back; the bridge never
//! opens a data folder.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::fetch;
use crate::json::{self, Json};

use super::rpc::{self, encode, obj, string};

pub const DEFAULT_URL: &str = "http://127.0.0.1:7476/mcp";

pub const USAGE: &str = "\
usage: graphbuff-server mcp [--url URL] [--cacert FILE]

MCP over standard input and output, for agents that start their MCP servers as programs
(Claude Desktop, for one). It passes each message to the MCP endpoint of a running
graphbuff-server ([mcp] bind in its config file), logged in with the API token in
env GRAPHBUFF_MCP_TOKEN.

  --url URL       the endpoint (default http://127.0.0.1:7476/mcp; also env GRAPHBUFF_MCP_URL)
  --cacert FILE   trust the certificates in FILE (PEM) for an https:// URL
                  (else env SSL_CERT_FILE, or the system's CA bundle on Linux and macOS)
  -h, --help      show this help";

/// JSON-RPC's code for an error of the server.
const UNAVAILABLE: i64 = -32000;
const PARSE_ERROR: i64 = -32700;

struct Bridge {
    url: String,
    token: String,
    /// The version `initialize` agreed on, sent with the later requests of a client
    /// that is not stateless.
    version: Mutex<Option<String>>,
    /// The connections of the requests in flight, by id, to close one when its request
    /// is cancelled.
    running: Mutex<HashMap<String, TcpStream>>,
    cancelled: Mutex<HashSet<String>>,
    out: Mutex<io::Stdout>,
}

/// Runs the bridge until its input ends.
pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut url = std::env::var("GRAPHBUFF_MCP_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| DEFAULT_URL.into());
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => url = args.next().ok_or("--url needs a URL")?,
            "--cacert" => fetch::set_ca_file(args.next().ok_or("--cacert needs a file")?.into())?,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unexpected `{other}`\n\n{USAGE}")),
        }
    }
    fetch::check_url(&url)?;
    let token = std::env::var("GRAPHBUFF_MCP_TOKEN").ok().filter(|t| !t.trim().is_empty()).ok_or(
        "set env GRAPHBUFF_MCP_TOKEN to an API token (CREATE TOKEN name ACCESS READ, in a GraphBuff shell)",
    )?;
    let bridge = Arc::new(Bridge {
        url,
        token: token.trim().to_string(),
        version: Mutex::new(None),
        running: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(HashSet::new()),
        out: Mutex::new(io::stdout()),
    });
    let mut threads = Vec::new();
    for line in io::stdin().lock().lines() {
        let line = line.map_err(|e| format!("cannot read the input: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let b = Arc::clone(&bridge);
        threads.push(std::thread::spawn(move || b.pass(&line)));
        threads.retain(|t| !t.is_finished());
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

impl Bridge {
    fn write(&self, message: &Json) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{}", encode(message));
        let _ = out.flush();
    }

    fn error(&self, id: &Json, code: i64, message: String) {
        self.write(&obj([
            ("jsonrpc", string("2.0")),
            ("id", id.clone()),
            ("error", obj([("code", Json::Int(code)), ("message", string(message))])),
        ]));
    }

    /// Sends one message on, and writes its answer.
    fn pass(&self, line: &str) {
        let msg = match json::parse(line) {
            Ok(Json::Object(m)) => m,
            Ok(_) => return self.error(&Json::Null, PARSE_ERROR, "a message must be a JSON object".into()),
            Err(e) => return self.error(&Json::Null, PARSE_ERROR, format!("not JSON: {e}")),
        };
        let method = match msg.get("method") {
            Some(Json::String(m)) => Some(m.as_str()),
            _ => None,
        };
        let id = msg.get("id").filter(|_| method.is_some()).cloned();
        let params = match msg.get("params") {
            Some(Json::Object(p)) => Some(p),
            _ => None,
        };
        if method == Some("notifications/cancelled") {
            // Closing the connection stops the request's query on the server.
            if let Some(Json::Object(p)) = msg.get("params")
                && let Some(r) = p.get("requestId")
            {
                let key = encode(r);
                self.cancelled.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone());
                if let Some(s) = self.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&key) {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
            return;
        }

        let mut headers: Vec<(&str, String)> = vec![("Authorization", format!("Bearer {}", self.token))];
        let modern = params
            .and_then(|p| p.get("_meta"))
            .and_then(|m| match m {
                Json::Object(m) => m.get(rpc::META_VERSION),
                _ => None,
            })
            .and_then(|v| match v {
                Json::String(v) => Some(v.clone()),
                _ => None,
            });
        match &modern {
            Some(v) => {
                headers.push(("MCP-Protocol-Version", v.clone()));
                if let Some(m) = method {
                    headers.push(("Mcp-Method", m.to_string()));
                    let field = match m {
                        "tools/call" | "prompts/get" => Some("name"),
                        "resources/read" => Some("uri"),
                        _ => None,
                    };
                    if let Some(Json::String(name)) = field.and_then(|f| params.and_then(|p| p.get(f))) {
                        headers.push(("Mcp-Name", header_value(name)));
                    }
                }
            }
            None => {
                if method != Some("initialize")
                    && let Some(v) = self.version.lock().unwrap_or_else(|e| e.into_inner()).clone()
                {
                    headers.push(("MCP-Protocol-Version", v));
                }
            }
        }
        let key = id.as_ref().map(encode);
        let answer = fetch::post(&self.url, &headers, line.as_bytes(), &mut |tcp| {
            if let (Some(k), Ok(s)) = (&key, tcp.try_clone()) {
                self.running.lock().unwrap_or_else(|e| e.into_inner()).insert(k.clone(), s);
            }
        });
        if let Some(k) = &key {
            self.running.lock().unwrap_or_else(|e| e.into_inner()).remove(k);
            if self.cancelled.lock().unwrap_or_else(|e| e.into_inner()).remove(k) {
                return;
            }
        }
        let Some(id) = id else { return };
        match answer {
            Err(e) => self.error(
                &id,
                UNAVAILABLE,
                format!(
                    "cannot reach GraphBuff's MCP endpoint at {} ({e}). Start graphbuff-server with `bind` set in \
                     the [mcp] section of its config file, or pass the endpoint with --url",
                    self.url
                ),
            ),
            Ok((status, body)) => match json::parse(&String::from_utf8_lossy(&body)) {
                Ok(Json::Object(mut m)) => {
                    if method == Some("initialize")
                        && let Some(Json::Object(r)) = m.get("result")
                        && let Some(Json::String(v)) = r.get("protocolVersion")
                    {
                        *self.version.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.clone());
                    }
                    // An error the server could not tie to the request (a refused login).
                    if matches!(m.get("id"), None | Some(Json::Null)) {
                        m.insert("id".into(), id.clone());
                        if status == 401
                            && let Some(Json::Object(e)) = m.get_mut("error")
                            && let Some(Json::String(msg)) = e.get_mut("message")
                        {
                            msg.push_str(" (the bridge sends the token in env GRAPHBUFF_MCP_TOKEN)");
                        }
                    }
                    self.write(&Json::Object(m))
                }
                _ => {
                    let text = String::from_utf8_lossy(&body);
                    let text = text.trim();
                    let mut message = format!("GraphBuff answered HTTP {status}");
                    if !text.is_empty() {
                        message.push_str(": ");
                        message.push_str(&text.chars().take(300).collect::<String>());
                    }
                    if status == 401 {
                        message.push_str(" (check the API token in GRAPHBUFF_MCP_TOKEN)");
                    }
                    self.error(&id, UNAVAILABLE, message)
                }
            },
        }
    }
}

/// A header value: as it is if it is printable ASCII, else `=?base64?…?=`.
fn header_value(v: &str) -> String {
    if v.bytes().all(|b| (0x20..0x7f).contains(&b)) && v.trim() == v {
        v.to_string()
    } else {
        let mut b64 = crate::crypto::base64_encode(v.as_bytes());
        while !b64.len().is_multiple_of(4) {
            b64.push('=');
        }
        format!("=?base64?{b64}?=")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn header_values() {
        assert_eq!(super::header_value("read_cypher"), "read_cypher");
        assert_eq!(super::header_value("café"), "=?base64?Y2Fmw6k=?=");
        assert_eq!(super::header_value(" x"), "=?base64?IHg=?=");
    }
}
