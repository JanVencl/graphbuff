# graphbuff-wasm

The graphbuff engine compiled to WebAssembly, for running Cypher in a browser with no
server: an in-memory database with the movies demo graph.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo build -p graphbuff-wasm --target wasm32-unknown-unknown --profile wasm
cp target/wasm32-unknown-unknown/wasm/graphbuff_wasm.wasm crates/wasm/www/graphbuff.wasm
```

No other tool is needed: the module exports plain functions (`gb_open`, `gb_run`, ...)
and `www/graphbuff.js` wraps them.

## Try it

Serve `crates/wasm/www` over HTTP (browsers do not load modules from `file://`), for
example `python -m http.server --directory crates/wasm/www 8765`, and open
`http://localhost:8765/`. The page has a query box, a result table and a self-test.

## From JavaScript

```js
import { loadGraphbuff } from "./graphbuff.js";

const gb = await loadGraphbuff("graphbuff.wasm");
const db = gb.open();
db.run("MATCH (p:Person) WHERE p.born < $year RETURN p.name", { year: 1965 });
// {columns: [...], rows: [[...]], stats: {...}}  or  {error: {code, message, offset?}}
```

The result has the shape of the HTTP API of `graphbuff-server`.

## What a browser does not have

* Files: persistent and hybrid storage, `LOAD CSV`, CSV import, backup and restore.
* Threads: kernels run on the calling thread.
* A way to stop a running query from outside; a query runs to its end on the thread that
  called it. Run the module in a Web Worker to keep a page responsive.
