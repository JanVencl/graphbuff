//! JSON-RPC messages of the Model Context Protocol, in both eras: 2026-07-28 (every
//! request says its version in `_meta` and in HTTP headers) and the `initialize`-based
//! 2025-06-18 and 2025-11-25.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::json::{self, Json};

/// The stateless protocol version.
pub(crate) const MODERN: &str = "2026-07-28";
/// The `initialize`-based versions, newest first.
pub(crate) const LEGACY: [&str; 2] = ["2025-11-25", "2025-06-18"];
/// What a legacy client without `MCP-Protocol-Version` speaks (by the 2025 specs). It is
/// served as a legacy client; `initialize` offers it [`LEGACY`]`[0]`.
const ASSUMED: &str = "2025-03-26";

pub(crate) const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub(crate) const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const HEADER_MISMATCH: i64 = -32020;
pub(crate) const UNSUPPORTED_VERSION: i64 = -32022;
/// "Resource not found" before 2026-07-28 (which uses [`INVALID_PARAMS`]).
pub(crate) const LEGACY_RESOURCE_NOT_FOUND: i64 = -32002;

/// A request: it has an `id`, so it gets a response.
#[derive(Debug)]
pub(crate) struct Message {
    pub id: Json,
    pub method: String,
    pub params: BTreeMap<String, Json>,
}

#[derive(Debug)]
pub(crate) enum Parsed {
    Request(Message),
    /// A notification (no `id`): answered with `202` and no body.
    Notification,
    /// A response from the client (to nothing: this server sends no requests): `202` too.
    Response,
}

/// A JSON-RPC error, and the HTTP status it is sent with.
#[derive(Debug)]
pub(crate) struct RpcError {
    pub status: u16,
    pub code: i64,
    pub message: String,
    pub data: Option<Json>,
}

impl RpcError {
    pub(crate) fn new(status: u16, code: i64, message: impl Into<String>) -> RpcError {
        RpcError { status, code, message: message.into(), data: None }
    }
}

/// Which protocol era a request belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Era {
    /// 2026-07-28: no session, version on every request.
    Modern,
    /// 2025-06-18 or 2025-11-25 (or an older client without a version header).
    Legacy,
}

/// Reads one JSON-RPC message. Batches (arrays) are refused, as by MCP since 2025-06-18.
pub(crate) fn parse(body: &str) -> Result<Parsed, RpcError> {
    let invalid = |m: &str| RpcError::new(400, INVALID_REQUEST, m);
    let j = json::parse(body).map_err(|e| RpcError::new(400, PARSE_ERROR, format!("the body is not JSON: {e}")))?;
    let mut m = match j {
        Json::Object(m) => m,
        Json::Array(_) => return Err(invalid("batches are not supported: send one JSON-RPC message per request")),
        _ => return Err(invalid("a JSON-RPC message is a JSON object")),
    };
    if !matches!(m.get("jsonrpc"), Some(Json::String(v)) if v == "2.0") {
        return Err(invalid("`jsonrpc` must be \"2.0\""));
    }
    let id = match m.remove("id") {
        None => None,
        Some(id @ (Json::String(_) | Json::Int(_))) => Some(id),
        Some(_) if !m.contains_key("method") => return Ok(Parsed::Response),
        Some(_) => return Err(invalid("`id` must be a string or an integer")),
    };
    let method = match m.remove("method") {
        Some(Json::String(s)) => s,
        Some(_) => return Err(invalid("`method` must be a string")),
        None if m.contains_key("result") || m.contains_key("error") => return Ok(Parsed::Response),
        None => return Err(invalid("missing `method`")),
    };
    let params = match m.remove("params") {
        None => BTreeMap::new(),
        Some(Json::Object(p)) => p,
        Some(_) => return Err(RpcError::new(400, INVALID_PARAMS, "`params` must be an object")),
    };
    Ok(match id {
        None => Parsed::Notification,
        Some(id) => Parsed::Request(Message { id, method, params }),
    })
}

/// The era of `msg`, checking what that era asks of a request: in 2026-07-28 the
/// `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` headers must repeat the body, and
/// `_meta` must name the client's capabilities. `headers` are lower-cased names.
pub(crate) fn era(msg: &Message, headers: &[(String, String)]) -> Result<Era, RpcError> {
    let header = |n: &str| headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    if msg.method == "initialize" {
        return Ok(Era::Legacy);
    }
    let meta = match msg.params.get("_meta") {
        None => None,
        Some(Json::Object(m)) => Some(m),
        Some(_) => return Err(RpcError::new(400, INVALID_PARAMS, "`_meta` must be an object")),
    };
    let sent = header("mcp-protocol-version");
    let Some(version) = meta.and_then(|m| m.get(META_VERSION)) else {
        return match sent {
            None => Ok(Era::Legacy),
            Some(v) if LEGACY.contains(&v) || v == ASSUMED => Ok(Era::Legacy),
            Some(MODERN) => Err(RpcError::new(
                400,
                INVALID_PARAMS,
                format!("protocol version {MODERN} needs `_meta` \"{META_VERSION}\" on every request"),
            )),
            Some(v) => Err(unsupported(v)),
        };
    };
    let Json::String(version) = version else {
        return Err(RpcError::new(400, INVALID_PARAMS, format!("`_meta` \"{META_VERSION}\" must be a string")));
    };
    if version != MODERN {
        return Err(unsupported(version));
    }
    let mismatch = |name: &str| {
        Err(RpcError::new(400, HEADER_MISMATCH, format!("Header mismatch: {name} is missing or does not match the body")))
    };
    if sent != Some(MODERN) {
        return mismatch("MCP-Protocol-Version");
    }
    if header("mcp-method") != Some(msg.method.as_str()) {
        return mismatch("Mcp-Method");
    }
    let name_field = match msg.method.as_str() {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    };
    if let Some(field) = name_field {
        let Some(Json::String(want)) = msg.params.get(field) else {
            return Err(RpcError::new(400, INVALID_PARAMS, format!("{} needs `{field}` (a string)", msg.method)));
        };
        if header("mcp-name").and_then(decode_header).as_deref() != Some(want.as_str()) {
            return mismatch("Mcp-Name");
        }
    }
    if !matches!(meta.and_then(|m| m.get(META_CAPABILITIES)), Some(Json::Object(_))) {
        return Err(RpcError::new(
            400,
            INVALID_PARAMS,
            format!("`_meta` \"{META_CAPABILITIES}\" (an object) is required"),
        ));
    }
    Ok(Era::Modern)
}

/// Every version served, newest first.
pub(crate) fn supported() -> Json {
    Json::Array(std::iter::once(MODERN).chain(LEGACY).map(|v| Json::String(v.into())).collect())
}

fn unsupported(requested: &str) -> RpcError {
    RpcError {
        data: Some(obj([("supported", supported()), ("requested", Json::String(requested.into()))])),
        ..RpcError::new(400, UNSUPPORTED_VERSION, "Unsupported protocol version")
    }
}

/// A header value: as sent, or decoded from `=?base64?…?=`.
fn decode_header(v: &str) -> Option<String> {
    match v.strip_prefix("=?base64?").and_then(|r| r.strip_suffix("?=")) {
        Some(b64) => String::from_utf8(crate::crypto::base64_decode(b64)?).ok(),
        None => Some(v.to_string()),
    }
}

/// The version `initialize` answers a client that asked for `requested`: that one if it
/// is served, else the newest legacy one.
pub(crate) fn negotiate(requested: Option<&str>) -> &'static str {
    LEGACY.into_iter().find(|v| Some(*v) == requested).unwrap_or(LEGACY[0])
}

// ---- writing -------------------------------------------------------------------------------

/// A JSON object of `pairs`.
pub(crate) fn obj<const N: usize>(pairs: [(&str, Json); N]) -> Json {
    Json::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub(crate) fn string(s: impl Into<String>) -> Json {
    Json::String(s.into())
}

/// Writes `j` as compact JSON.
pub(crate) fn write(out: &mut String, j: &Json) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Json::Float(f) => json::write_float(out, *f),
        Json::String(s) => json::write_str(out, s),
        Json::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(out, v);
            }
            out.push(']');
        }
        Json::Object(m) => {
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json::write_str(out, k);
                out.push(':');
                write(out, v);
            }
            out.push('}');
        }
    }
}

pub(crate) fn encode(j: &Json) -> String {
    let mut out = String::new();
    write(&mut out, j);
    out
}

/// A response with `result`.
pub(crate) fn success(id: &Json, result: &Json) -> String {
    let mut out = String::from(r#"{"jsonrpc":"2.0","id":"#);
    write(&mut out, id);
    out.push_str(r#","result":"#);
    write(&mut out, result);
    out.push('}');
    out
}

/// An error response; `id` is `null` when the request's is not known.
pub(crate) fn failure(id: Option<&Json>, e: &RpcError) -> String {
    let mut error = BTreeMap::from([
        ("code".to_string(), Json::Int(e.code)),
        ("message".to_string(), Json::String(e.message.clone())),
    ]);
    if let Some(d) = &e.data {
        error.insert("data".into(), d.clone());
    }
    let mut out = String::from(r#"{"jsonrpc":"2.0","id":"#);
    write(&mut out, id.unwrap_or(&Json::Null));
    out.push_str(r#","error":"#);
    write(&mut out, &Json::Object(error));
    out.push('}');
    out
}

/// Parses `data` as a message and finds its era with no headers (see [`crate::fuzzing`]).
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_message(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(Parsed::Request(m)) = parse(text) {
        let _ = era(&m, &[]);
        let _ = era(&m, &[("mcp-protocol-version".into(), MODERN.into()), ("mcp-method".into(), m.method.clone())]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: &str) -> Message {
        match parse(body).unwrap() {
            Parsed::Request(m) => m,
            other => panic!("{other:?}"),
        }
    }

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

    #[test]
    fn messages() {
        assert!(matches!(parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), Ok(Parsed::Notification)));
        assert!(matches!(parse(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#), Ok(Parsed::Response)));
        let m = request(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#);
        assert_eq!((m.id, m.method.as_str(), m.params.len()), (Json::String("a".into()), "tools/list", 0));
        let code = |b: &str| parse(b).unwrap_err().code;
        assert_eq!(code("{"), PARSE_ERROR);
        assert_eq!(code("[]"), INVALID_REQUEST);
        assert_eq!(code(r#"{"id":1,"method":"x"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1.5,"method":"x"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"x","params":[]}"#), INVALID_PARAMS);
    }

    #[test]
    fn eras_and_headers() {
        let legacy = request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        assert_eq!(era(&legacy, &[]).unwrap(), Era::Legacy);
        assert_eq!(era(&legacy, &h(&[("mcp-protocol-version", "2025-06-18")])).unwrap(), Era::Legacy);
        assert_eq!(era(&legacy, &h(&[("mcp-protocol-version", "2026-07-28")])).unwrap_err().code, INVALID_PARAMS);
        let e = era(&legacy, &h(&[("mcp-protocol-version", "1900-01-01")])).unwrap_err();
        assert_eq!((e.status, e.code), (400, UNSUPPORTED_VERSION));
        assert_eq!(encode(e.data.as_ref().unwrap()), r#"{"requested":"1900-01-01","supported":["2026-07-28","2025-11-25","2025-06-18"]}"#);
        let init = request(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
        assert_eq!(era(&init, &[]).unwrap(), Era::Legacy);

        let modern = request(&format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#));
        let ok = h(&[("mcp-protocol-version", "2026-07-28"), ("mcp-method", "tools/list")]);
        assert_eq!(era(&modern, &ok).unwrap(), Era::Modern);
        assert_eq!(era(&modern, &ok[..1]).unwrap_err().code, HEADER_MISMATCH);
        assert_eq!(era(&modern, &ok[1..]).unwrap_err().code, HEADER_MISMATCH);
        let wrong = h(&[("mcp-protocol-version", "2026-07-28"), ("mcp-method", "tools/call")]);
        assert_eq!(era(&modern, &wrong).unwrap_err().code, HEADER_MISMATCH);
        let no_caps = request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#);
        assert_eq!(era(&no_caps, &ok).unwrap_err().code, INVALID_PARAMS);
        let old = request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2025-11-25"}}}"#);
        assert_eq!(era(&old, &ok).unwrap_err().code, UNSUPPORTED_VERSION);

        let call = request(&format!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"list_databases",{META}}}}}"#));
        let mut headers = h(&[("mcp-protocol-version", "2026-07-28"), ("mcp-method", "tools/call")]);
        assert_eq!(era(&call, &headers).unwrap_err().code, HEADER_MISMATCH);
        headers.push(("mcp-name".into(), "list_databases".into()));
        assert_eq!(era(&call, &headers).unwrap(), Era::Modern);
        // "list_databases" in base64.
        headers[2].1 = "=?base64?bGlzdF9kYXRhYmFzZXM=?=".into();
        assert_eq!(era(&call, &headers).unwrap(), Era::Modern);
        headers[2].1 = "other".into();
        assert_eq!(era(&call, &headers).unwrap_err().code, HEADER_MISMATCH);
    }

    #[test]
    fn negotiation_and_writing() {
        assert_eq!(negotiate(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate(Some("2025-03-26")), "2025-11-25");
        assert_eq!(negotiate(None), "2025-11-25");
        let e = RpcError::new(404, METHOD_NOT_FOUND, "no \"x\"");
        assert_eq!(failure(None, &e), r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"no \"x\""}}"#);
        let r = obj([("b", Json::Array(vec![Json::Int(1), Json::Float(0.5), Json::Null])), ("a", Json::Bool(true))]);
        assert_eq!(success(&Json::String("q".into()), &r), r#"{"jsonrpc":"2.0","id":"q","result":{"a":true,"b":[1,0.5,null]}}"#);
    }
}
