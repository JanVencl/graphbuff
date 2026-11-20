//! A Bolt message (PackStream) and the conversion of its fields to query values.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    graphbuff_server::fuzzing::bolt_message(data);
});
