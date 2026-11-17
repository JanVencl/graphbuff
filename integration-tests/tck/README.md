# openCypher TCK

Runs the [openCypher TCK](https://github.com/opencypher/openCypher/tree/main/tck) (Technology
Compatibility Kit) against `graphbuff-server` over HTTP. It's driven by behave (Python Cucumber).

`features/` and `graphs/` are the TCK of openCypher release 2024.3, copied unchanged
(Apache License 2.0: [LICENSE-TCK](LICENSE-TCK), [NOTICE-TCK](NOTICE-TCK)).

```bash
cargo build --release -p graphbuff-server
pip install -r integration-tests/requirements.txt
cd integration-tests/tck
python run.py                    # pass rate per feature group
python run.py --features         # ... per feature file
python run.py --failures         # ... plus every failing scenario and why
python run.py -v                 # live line per scenario (default: per feature file; -q: none)
python run.py -- features/clauses/match/Match1.feature   # one file (any behave arguments)
```

The suite uses the server binary named by `GRAPHBUFF_SERVER`. Without it, the suite uses whichever of
`target/release` and `target/debug` was built last. Build the release server before you measure
the pass rate: the debug build overflows its stack on queries that the release build runs.

`passing.txt` lists the scenarios that pass. A full run exits 1 if one of them no longer
passes. After a change that makes more scenarios pass, record them with
`python run.py --save-baseline`. Each run writes every scenario's outcome to
`results/results.tsv` (git-ignored).

## How scenarios run

- One server for the whole run. Each scenario gets a fresh in-memory database
  (`CREATE OR REPLACE DATABASE tck`). If a query crashes the server, that scenario fails
  with the server's exit message, and the server restarts for the next scenario.
- Results are compared value by value. Lists keep their order unless the step says to
  ignore it, and rows keep their order only for `in order`. Nodes compare by labels and
  properties, relationships by type and properties, and paths also by direction. Temporal
  values compare as their ISO 8601 strings, which is how the TCK writes them.
- Side effects are computed the way the TCK's reference driver computes them: by diffing
  the graph before and after the query. The diff covers node and relationship ids, the
  label names in use, and every property as an (entity, key, value) triple.
- Errors are checked by category, not by detail code (`VariableTypeConflict`, …) or phase
  (compile time or runtime). The TCK files every compile-time error under `SyntaxError`,
  and graphbuff reports these as `SyntaxError` or `SemanticError`, so either one counts.
  `ArgumentError` accepts `RuntimeError` or `TypeError`. `Unsupported` never counts as an
  expected error.
- Test procedures (`there exists a procedure …`, in `clauses/call`) are added through
  `POST /db/{name}/test-procedure`, which the server offers only when started with
  `--test-procedures`. Each answers from its table: the rows whose input columns equal
  the arguments. Argument types are not checked.
