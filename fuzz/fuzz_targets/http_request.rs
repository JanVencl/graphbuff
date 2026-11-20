//! HTTP/1.1 requests on one connection: request line, headers, Content-Length and
//! chunked bodies.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    graphbuff_server::fuzzing::http_request(data);
});
