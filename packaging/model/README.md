# Embedding models

graphbuff-server loads text embedding models for `vector.embed()` from this folder. Put
each model in a sub-folder; the sub-folder's name is the model's name. The models are
found when the server starts.

```
model/
  minilm/
    model.toml          # what graphbuff needs to know (see below)
    config.json         # from the model's Hugging Face repository
    tokenizer.json      # from the model's Hugging Face repository
    model.safetensors   # from the model's Hugging Face repository
```

The easiest way to get a model is the script in this folder (Python 3.8+, no packages):

```
python fetch-model.py --list
python fetch-model.py minilm
```

To use another folder, set `model_dir` in the `[embedding]` section of
`graphbuff.toml`.

`model.toml`:

```toml
architecture = "bert"   # BERT-family models: MiniLM, BGE, E5, GTE
dimensions   = 384      # the model's hidden_size
max_tokens   = 256      # longer texts are cut (default: the model's limit)
pooling      = "mean"   # mean or cls, as the model card says
normalize    = true
query_prefix = ""       # e.g. "query: " for E5, used with {task: 'query'}
doc_prefix   = ""       # e.g. "passage: " for E5
```

See docs/Embeddings.md in the graphbuff repository for details.
