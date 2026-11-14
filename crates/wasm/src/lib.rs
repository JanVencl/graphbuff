//! graphbuff in the browser: an in-memory database behind a small C interface, built for
//! `wasm32-unknown-unknown` and driven from JavaScript (see `www/graphbuff.js`).
//!
//! A query answers with the JSON the HTTP API of `graphbuff-server` sends:
//! `{"columns": [...], "rows": [[...]], "stats": {...}}`, or
//! `{"error": {"code": "...", "message": "..."}}` (with `"offset"` for a syntax error).
//!
//! The interface is plain exported functions over the module's memory, without a binding
//! generator, so the crate has no dependency besides the engine. Text crosses as UTF-8:
//! JavaScript writes it into a buffer from [`gb_alloc`], and reads the answer of
//! [`gb_run`] from [`gb_result`].

use graphbuff::json::{self, Json};
use graphbuff::{DatabaseManager, Error};

/// An in-memory database with a demo graph loaded.
pub struct Playground {
    mgr: DatabaseManager,
}

impl Playground {
    /// A new database holding the demo graph `demo` (see [`graphbuff::DEMOS`]).
    pub fn with_demo(demo: &str) -> Result<Playground, Error> {
        let mgr = DatabaseManager::in_memory();
        graphbuff::load_demo(&mgr, demo)?;
        Ok(Playground { mgr })
    }

    /// Runs `query` with the parameters in `params` (a JSON object; empty or `null` for
    /// none), and returns the result or the error as JSON.
    pub fn run(&self, query: &str, params: &str) -> String {
        let params = if params.trim().is_empty() {
            Default::default()
        } else {
            match json::parse(params) {
                Ok(Json::Null) => Default::default(),
                Ok(Json::Object(m)) => m.into_iter().map(|(k, v)| (k, json::to_value(v))).collect(),
                Ok(other) => {
                    let message = format!("`params` must be an object, got {}", other.type_name());
                    return json::encode_error_message("BadRequest", &message);
                }
                Err(e) => return json::encode_error_message("BadRequest", &e.to_string()),
            }
        };
        match self.mgr.session().execute_with_params(query, &params) {
            Ok(r) => json::encode_result(&r),
            Err(e) => json::encode_error(&e),
        }
    }
}

/// What JavaScript holds: the database and the answer to its last query.
pub struct Handle {
    playground: Playground,
    result: String,
}

/// Allocates `len` bytes for the caller to fill; free them with [`gb_free`].
#[unsafe(no_mangle)]
pub extern "C" fn gb_alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Frees a buffer from [`gb_alloc`].
///
/// # Safety
/// `ptr` must come from `gb_alloc(len)` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gb_free(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
}

/// Opens an in-memory database with the movies demo graph. Close it with [`gb_close`].
#[unsafe(no_mangle)]
pub extern "C" fn gb_open() -> *mut Handle {
    match Playground::with_demo("movies") {
        Ok(playground) => Box::into_raw(Box::new(Handle { playground, result: String::new() })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Closes a database from [`gb_open`].
///
/// # Safety
/// `handle` must come from `gb_open` and not have been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gb_close(handle: *mut Handle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// Runs the query in `query[..query_len]` with the JSON parameters in
/// `params[..params_len]` (both UTF-8) and returns the length in bytes of the answer,
/// which [`gb_result`] points to.
///
/// # Safety
/// `handle` must be open, and both buffers readable for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gb_run(
    handle: *mut Handle,
    query: *const u8,
    query_len: usize,
    params: *const u8,
    params_len: usize,
) -> usize {
    let handle = unsafe { &mut *handle };
    let text = |ptr: *const u8, len: usize| {
        let bytes = if len == 0 { &[][..] } else { unsafe { std::slice::from_raw_parts(ptr, len) } };
        std::str::from_utf8(bytes)
    };
    handle.result = match (text(query, query_len), text(params, params_len)) {
        (Ok(query), Ok(params)) => handle.playground.run(query, params),
        _ => json::encode_error_message("BadRequest", "the query and parameters must be UTF-8"),
    };
    handle.result.len()
}

/// The answer to the last [`gb_run`] on `handle`, valid until the next one.
///
/// # Safety
/// `handle` must be open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gb_result(handle: *const Handle) -> *const u8 {
    unsafe { (*handle).result.as_ptr() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(text: &str) -> std::collections::BTreeMap<String, Json> {
        match json::parse(text) {
            Ok(Json::Object(m)) => m,
            other => panic!("{other:?}: {text}"),
        }
    }

    #[test]
    fn runs_queries_on_the_demo_graph() {
        let p = Playground::with_demo("movies").unwrap();
        let r = object(&p.run("MATCH (m:Movie) RETURN count(*) AS movies", ""));
        assert_eq!(r["columns"], Json::Array(vec![Json::String("movies".into())]));
        assert_eq!(r["rows"], Json::Array(vec![Json::Array(vec![Json::Int(3)])]));
        assert!(matches!(r["stats"], Json::Object(_)));
    }

    #[test]
    fn writes_stay_and_params_apply() {
        let p = Playground::with_demo("movies").unwrap();
        let r = object(&p.run("CREATE (:Person {name: $name, born: $born})", r#"{"name": "Ann", "born": 1990}"#));
        let Json::Object(stats) = &r["stats"] else { panic!() };
        assert_eq!(stats["nodesCreated"], Json::Int(1));
        let r = object(&p.run("MATCH (p:Person) WHERE p.born > $y RETURN p.name", r#"{"y": 1980}"#));
        assert_eq!(r["rows"], Json::Array(vec![Json::Array(vec![Json::String("Ann".into())])]));
    }

    #[test]
    fn errors_are_json() {
        let p = Playground::with_demo("movies").unwrap();
        let r = object(&p.run("MATCH (n RETURN n", ""));
        let Json::Object(e) = &r["error"] else { panic!() };
        assert_eq!(e["code"], Json::String("SyntaxError".into()));
        assert!(matches!(e["offset"], Json::Int(_)));
        for params in ["[1]", "{"] {
            let r = object(&p.run("RETURN 1", params));
            let Json::Object(e) = &r["error"] else { panic!() };
            assert_eq!(e["code"], Json::String("BadRequest".into()));
        }
    }

    #[test]
    fn the_c_interface_round_trips() {
        let (query, params) = ("RETURN $x + 1 AS y", r#"{"x": 41}"#);
        unsafe {
            let h = gb_open();
            let q = gb_alloc(query.len());
            std::ptr::copy_nonoverlapping(query.as_ptr(), q, query.len());
            let len = gb_run(h, q, query.len(), params.as_ptr(), params.len());
            let out = std::str::from_utf8(std::slice::from_raw_parts(gb_result(h), len)).unwrap().to_string();
            gb_free(q, query.len());
            gb_close(h);
            assert_eq!(object(&out)["rows"], Json::Array(vec![Json::Array(vec![Json::Int(42)])]));
        }
    }
}
