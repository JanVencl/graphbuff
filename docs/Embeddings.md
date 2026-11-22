# Text embeddings

graphbuff-server can turn text into vectors itself, with local models, so you can embed,
store and search without leaving Cypher and without an outside service:

```cypher
MATCH (d:Doc) SET d.embedding = vector.embed(d.text)
```

The vectors are ordinary `LIST<FLOAT>` properties, so they go straight into a
[vector index](Vectors.md).

- [Setting up](#setting-up)
- [Functions and procedures](#functions-and-procedures)
- [Embed, index, search](#embed-index-search)
- [Models](#models)
- [Configuration](#configuration)
- [Performance](#performance)
- [How it works](#how-it-works)
- [Limits](#limits)

## Setting up

1. Use a server built with the `embed` feature. The release archives and the Docker
   image have it. From source: `cargo build --release -p graphbuff-server --features embed`.
2. Put one or more models in the model folder. By default that is `model` next to
   `graphbuff.toml` (in the release archives: `config/model/`). The script there fetches
   a known model from Hugging Face:

   ```
   python config/model/fetch-model.py --list
   python config/model/fetch-model.py minilm
   ```

   In Docker, the folder is `/models`. Mount yours with `-v ./model:/models:ro`.
3. Restart the server. Its log lists the models it found and any it skipped:

   ```
   INFO  embedding models in …/config/model: minilm (384 dimensions); default: minilm; device: cpu
   ```

`CALL vector.models()` lists them from Cypher.

## Functions and procedures

| | |
|---|---|
| `vector.embed(text, model?, options?)` → `LIST<FLOAT>` | The embedding of one text, in any expression |
| `CALL vector.embedBatch(texts, model?, options?) YIELD index, text, vector` | Many texts in batches: one row per text, in order |
| `CALL vector.models() YIELD name, dimensions, maxTokens, loaded` | The installed models |

- `model` is the name of a model (its folder). `null` or leaving it out uses the default
  model: `embedding.default_model`, or the only installed model. With several models and
  no default, name one.
- `options` is a map:
  - `task`: `'document'` (the default) or `'query'`. Some models want a different
    prefix on search queries than on documents (E5 uses `query: ` and `passage: `). The
    model's `model.toml` sets the prefixes, and `task` picks one.
  - `truncate`: `true` by default, which cuts text longer than the model's `maxTokens`.
    With `false`, such text is an error.
- A `null` text gives a `null` vector. `embedBatch` keeps `null` entries, with a `null`
  vector.
- Errors:
  - An unknown model, too-long text with `truncate: false`, or no default model is a
    client error.
  - So is any call to a server without models: *embedding is not available*.
  - A model that fails to load is a runtime error.

`vector.embed` runs once per row. To embed many texts, `vector.embedBatch` is faster: it
sorts the texts by length and runs them through the model in batches, about twice the
throughput for short texts (see [Performance](#performance)).

## Embed, index, search

```cypher
// Embed all documents in batches and store the vectors.
MATCH (d:Doc) WHERE d.embedding IS NULL
WITH collect(d) AS docs
CALL vector.embedBatch([d IN docs | d.text]) YIELD index, vector
WITH docs[index] AS d, vector
SET d.embedding = vector;

// Index them: the dimensions must be the model's.
CREATE VECTOR INDEX doc_embedding IF NOT EXISTS
FOR (d:Doc) ON d.embedding
OPTIONS {indexConfig: {`vector.dimensions`: 384, `vector.similarity_function`: 'cosine'}};

// Search with the embedding of the question.
CALL db.index.vector.queryNodes('doc_embedding', 5, vector.embed($question, null, {task: 'query'}))
YIELD node, score
RETURN node.text, score;
```

One `collect` holds all the documents at once. For many documents, embed them in pages
and run the query until it sets nothing:

```cypher
MATCH (d:Doc) WHERE d.embedding IS NULL
WITH d LIMIT 1000
WITH collect(d) AS docs
CALL vector.embedBatch([d IN docs | d.text]) YIELD index, vector
WITH docs[index] AS d, vector
SET d.embedding = vector
```

Search with the model the documents were embedded with. Vectors of different models
cannot be compared, even when their lengths happen to match.

## Models

A model is a folder with four files:

| File | |
|---|---|
| `model.toml` | What graphbuff needs to know (below) |
| `config.json` | The model's Hugging Face config: layers, heads, sizes |
| `tokenizer.json` | The model's Hugging Face tokenizer |
| `model.safetensors` | The weights, as Hugging Face publishes them (F32) |

```toml
architecture = "bert"   # required; the only one for now
dimensions   = 384      # required; must equal hidden_size in config.json
max_tokens   = 256      # longer texts are cut; default: the model's max_position_embeddings
pooling      = "mean"   # mean or cls, as the model card says
normalize    = true     # L2-normalize: what cosine indexes expect
query_prefix = ""       # put before texts embedded with {task: 'query'}
doc_prefix   = ""       # put before documents
```

- The folder's name is the model's name: 1 to 64 of `a-z 0-9 . _ -`.
- `fetch-model.py` writes `model.toml` for the models it knows:

  | Name | Repository | Dims | Pooling | Notes |
  |---|---|---|---|---|
  | `minilm` | sentence-transformers/all-MiniLM-L6-v2 | 384 | mean | small and fast, English |
  | `bge-small-en` | BAAI/bge-small-en-v1.5 | 384 | cls | query prefix |
  | `e5-small` | intfloat/e5-small-v2 | 384 | mean | `query: ` / `passage: ` prefixes |

  For other BERT models use `--repo owner/name --pooling …`.
- BERT-family models work: BERT, MiniLM, BGE, E5 and GTE, small and base. Other
  architectures (XLM-RoBERTa, nomic-bert, decoder models) are not supported yet.
- At startup the server checks every folder: manifest, config, files, and that the
  dimensions match. A bad folder is skipped with a warning and the others still load.
  The weights are read on a model's first use.
- Vectors match Hugging Face `transformers` (sentence-transformers' pooling) to about
  1e-6. The GPU gives slightly different floats than the CPU.

## Configuration

The `[embedding]` section of `graphbuff.toml`:

| Setting | Default | |
|---|---|---|
| `model_dir` | `model` next to the file | The model folder. A missing default folder just turns embedding off; a `model_dir` you set must exist |
| `default_model` | the only model | Used when a query names none; must be installed |
| `batch_size` | 32 | Texts per forward pass |
| `threads` | 0 (half the cores) | Threads that run the models, apart from the query threads |
| `max_loaded_models` | 2 | Models kept in memory; the least recently used one is unloaded |
| `device` | `cpu` | `gpu` runs the models through wgpu (a build with `embed` and `gpu`) |

## Performance

all-MiniLM-L6-v2 on a 4-core desktop CPU, 2 threads (the default there). The numbers
come from `embedbench` (below):

| Texts | Batch | Texts/s | Tokens/s |
|---|---:|---:|---:|
| ~10 tokens | 1 | 87 | 930 |
| ~10 tokens | 8 | 204 | 2 200 |
| ~10 tokens | 64 | 216 | 2 300 |
| ~110 tokens | 8 | 17.5 | 1 960 |
| 256 tokens (cut) | 8 | 7.7 | 1 980 |

With 4 threads, short texts in batches of 32 reach about 270 texts/s (2 900 tokens/s).
In Cypher, 256 short texts: `vector.embed` per row 91 texts/s, `vector.embedBatch` 178
texts/s.

- Batching helps short texts most: one text at a time leaves the threads idle. Beyond
  a batch of 8, more threads help more than bigger batches. Long texts cost a little
  more per token, since attention grows with the square of the length.
- A model loads on its first use. For MiniLM (≈ 87 MiB of weights) that takes about
  0.1 s, or about 3 s with a cold disk cache. It then stays in memory for every query
  and database.
- For comparison, PyTorch on the same CPU with 4 threads is about 2× faster.

```
python benchmarks/run_all.py --only embedbench                  # with GRAPHBUFF_EMBED_BENCH_MODEL=path/to/model
cargo run --release -p graphbuff-embed --example embedbench -- --model path/to/model [--threads N] [--device both]
```

## How it works

- **Engine:** the engine itself runs no models. A server built with `embed` gives every
  database an embedder (`graphbuff-embed`). Embedded applications can set their own
  with `Database::set_embedder` or `DatabaseManager::set_embedder` and the
  `graphbuff::Embedder` trait.
- **Encoder:** a BERT encoder written in [Burn](https://burn.dev). It runs on Burn's
  pure-Rust CPU backend, or through wgpu on the GPU. Tokenizing uses the Hugging Face
  `tokenizers` crate in pure Rust.
- **Threads:** models run on their own thread pool (`embedding.threads`), so embedding
  does not take the threads of other queries.
- **Batching:** a call sorts its texts by length, embeds them in batches of
  `batch_size`, and returns them in the original order.
- **Memory:** loaded models are shared by all queries; their weights do not count
  towards a query's memory limit. While a batch runs, its estimated working memory does
  count, so `queries.max_memory_mb` stops a query that embeds too much at once.
- **Timeouts:** timeouts and cancellation (`TERMINATE TRANSACTIONS`, a closed
  connection) are checked between batches.
- **Durability:** the log and snapshots store the vectors a query wrote, not the call,
  so recovery runs no model, and a server without models reads them as usual.
- **Metrics:** `GET /metrics` shows `graphbuff_embed_calls_total`, `_texts_total`,
  `_tokens_total`, `_seconds_total`, `_model_loads_total`, `_model_load_seconds_total`,
  and `graphbuff_embed_model_loaded{model}`.

## Limits

- **Architectures:** BERT-family encoders only, with F32 safetensors weights. No
  quantized weights, no ONNX.
- **No downloads:** the server never downloads models. Use `fetch-model.py`, or copy
  the files yourself.
- **Long text:** text longer than the model's limit is cut (or refused), not split into
  chunks. Split long documents before embedding them.
- **No automatic embedding:** the vector is not computed when a node is written; set it
  in the query.
- **Text only:** no remote providers (OpenAI and the like), no images.
- **Not in the browser build.**
