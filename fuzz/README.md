# Fuzz targets

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets for the code that reads input
graphbuff does not control. Any input must be answered or refused with an error; a panic,
a hang or a runaway allocation is a bug.

| Target | Reads |
|--------|-------|
| `parse_statement` | Cypher statements (`graphbuff_cypher::parse_statement`) |
| `bolt_message` | Bolt messages: PackStream, and their fields as query values |
| `http_request` | HTTP/1.1 requests on one connection, `Content-Length` and chunked bodies |
| `storage` | hybrid snapshots and write-ahead log records, persistent metadata, records and B-tree pages, after their checksums matched |

The `fuzz` workflow runs every target for 10 minutes each night (and for 1 minute on a pull
request that changes `fuzz/`), keeping the corpus in the Actions cache. A crash fails the
run; its input is attached to the run as the `crash-<target>` artifact.

Locally (Linux, macOS or WSL; nightly Rust):

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
cargo +nightly fuzz run parse_statement fuzz/corpus/parse_statement fuzz/seeds/parse_statement
# reproduce a crash from the workflow's artifact
cargo +nightly fuzz run storage path/to/crash-<hash>
```

`seeds/` holds a few valid inputs per target to start from (committed); `corpus/` and
`artifacts/` are written by the fuzzer (not committed). A fixed crash gets a regular test
with its input.
