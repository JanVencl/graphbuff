//! Entry points for the fuzz targets in `fuzz/` (feature `fuzzing`). Each decodes bytes
//! the way a store reads them from disk, after the file's checksum matched: any input
//! must give a value or an error, never a panic.

/// The first byte picks the format, the rest is its body: a hybrid snapshot, a hybrid
/// write-ahead log record (applied to an empty graph), persistent metadata, or a
/// persistent record or B-tree page.
pub fn storage(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else { return };
    match which % 4 {
        0 => crate::hybrid::fuzz_snapshot(rest),
        1 => crate::hybrid::fuzz_wal_record(rest),
        2 => crate::persistent::fuzz_meta(rest),
        _ => crate::persistent::fuzz_page(rest),
    }
}
