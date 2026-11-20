//! The Cypher parser: any text a client sends parses or fails with an error.
#![no_main]

libfuzzer_sys::fuzz_target!(|text: &str| {
    let _ = graphbuff_cypher::parse_statement(text);
});
