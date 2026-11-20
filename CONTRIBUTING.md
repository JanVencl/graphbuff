# Contributing

Bug reports, fixes and improvements are welcome. For a security problem, follow
[SECURITY.md](SECURITY.md) instead of opening an issue.

## Issues

A useful bug report has the graphbuff version (or commit), the storage mode, the
smallest Cypher or steps that show the problem, and what you expected. For Cypher
behaviour, say what Neo4j does if you know.

For a larger change, open an issue first to agree on the approach.

## Building and testing

Rust 1.100 or newer.

```sh
cargo build                 # the default members: engine, server, CLI
cargo test                  # unit and integration tests
cargo clippy --all-targets  # should be clean
```

The GPU backend and the browser build are outside the default members:

```sh
cargo check -p graphbuff-gpu
cargo build -p graphbuff-wasm --target wasm32-unknown-unknown --profile wasm
```

Cypher changes should keep the openCypher TCK at 100 %: see
[integration-tests/tck](integration-tests/tck/README.md) (`python python/run_tck.py`
from `integration-tests`). Bolt changes can be checked against the official drivers with
the smoke suites in [integration-tests/drivers](integration-tests/drivers/README.md).

## Pull requests

- Branch from `main`; one topic per pull request.
- Add tests for what changes: a failing case first for a bug fix.
- Update the documentation (`README.md`, `docs/`) and the `Unreleased` section of
  [CHANGELOG.md](CHANGELOG.md) when behaviour visible to users changes.
- Keep commit messages in the repository's style: a short summary line, then what
  changed and why.

## License

graphbuff is licensed under the GNU Affero General Public License v3.0 only
(`AGPL-3.0-only`). By submitting a contribution you agree that it is licensed under the
same terms.
