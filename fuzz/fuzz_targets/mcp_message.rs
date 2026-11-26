//! MCP JSON-RPC messages: parsing, and the protocol era with and without headers.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    graphbuff_server::fuzzing::mcp_message(data);
});
