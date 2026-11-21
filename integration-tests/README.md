# Integration tests

| Folder | What |
|---|---|
| `python/` | All Python for the behave suites: hooks (`environment.py`), step definitions (`steps/`), server launcher, TCK value parser, TCK runner |
| `cucumber/features/` | graphbuff's own scenarios (`.feature` only) |
| `tck/` | openCypher TCK: `features/`, `graphs/`, `passing.txt` ([README](tck/README.md)) |
| `drivers/` | Bolt driver smoke suites, Python (pytest), Java (JUnit), JavaScript (node:test) and .NET (xUnit) ([README](drivers/README.md)) |

Both behave suites share one step registry. Hooks pick the server setup by feature path:
TCK features share one server (fresh database per scenario), all others get a fresh server
per scenario. `behave.ini` selects `python/runner.py`, which points behave at `python/`
instead of a `steps/` folder next to the features.

```bash
cargo build --release -p graphbuff-server
pip install -r integration-tests/python/requirements.txt
cd integration-tests
python -m behave                     # cucumber suite
python -m behave tck/features/clauses/match/Match1.feature   # any feature file
python python/run_tck.py             # TCK with pass rate and baseline check
```

Run from `integration-tests/` with `python -m behave` (it puts the folder on `sys.path`,
so behave can import `python.runner`).
