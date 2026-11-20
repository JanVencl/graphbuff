//! What the stores read from disk: hybrid snapshots and log records, persistent
//! metadata, records and B-tree pages (see `graphbuff_core::fuzzing::storage`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    graphbuff_core::fuzzing::storage(data);
});
