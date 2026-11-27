#!/usr/bin/env python3
"""Downloads a sentence-embedding model from Hugging Face into a graphbuff model folder.

    python fetch-model.py minilm                 # a known model, into the model folder
    python fetch-model.py --list
    python fetch-model.py my-model --repo org/name --pooling cls [--query-prefix "query: "]
    python fetch-model.py minilm --dir /path/to/model

It fetches config.json, tokenizer.json and model.safetensors of the repository and writes
model.toml, in <dir>/<name>/. Restart graphbuff-server to use the model (built with the
`embed` feature). BERT, XLM-RoBERTa and nomic-bert models with F32, F16 or BF16 safetensors
weights work; the architecture is read from config.json.

The default folder: the folder this script is in when that is called `model` (the release
archives ship it there), else ./model. HF_ENDPOINT changes the server (a mirror). Python 3.8+,
no packages needed.
"""

import argparse
import json
import os
import re
import shutil
import sys
import tempfile
import urllib.error
import urllib.request

# name: (repository, pooling, max_tokens, query_prefix, doc_prefix, matryoshka)
KNOWN = {
    "minilm": ("sentence-transformers/all-MiniLM-L6-v2", "mean", 256, "", "", None),
    "bge-small-en": ("BAAI/bge-small-en-v1.5", "cls", 512,
                     "Represent this sentence for searching relevant passages: ", "", None),
    "e5-small": ("intfloat/e5-small-v2", "mean", 512, "query: ", "passage: ", None),
    "multilingual-e5-small": ("intfloat/multilingual-e5-small", "mean", 512, "query: ", "passage: ", None),
    "multilingual-e5-base": ("intfloat/multilingual-e5-base", "mean", 512, "query: ", "passage: ", None),
    "nomic-embed-text": ("nomic-ai/nomic-embed-text-v1.5", "mean", 2048, "search_query: ", "search_document: ",
                         "layer-norm"),
}
# config.json's model_type: the architecture in model.toml.
ARCHITECTURES = {"bert": "bert", "xlm-roberta": "xlm-roberta", "nomic_bert": "nomic-bert"}


def model_shape(config):
    """(architecture, dimensions, the most tokens it takes) from config.json."""
    arch = ARCHITECTURES.get(config.get("model_type"))
    if arch is None:
        raise ValueError(f"model_type {config.get('model_type')!r} is not supported "
                         f"(only {', '.join(ARCHITECTURES)})")
    if arch == "nomic-bert":
        return arch, config["n_embd"], min(config.get("max_trained_positions", config["n_positions"]), config["n_positions"])
    limit = config["max_position_embeddings"]
    if arch == "xlm-roberta":
        limit -= config.get("pad_token_id", 1) + 1  # positions start after the padding id
    return arch, config["hidden_size"], limit
FILES = ("config.json", "tokenizer.json", "model.safetensors")
NAME = re.compile(r"^[a-z0-9][a-z0-9._-]{0,63}$")


def default_dir():
    here = os.path.dirname(os.path.abspath(__file__))
    return here if os.path.basename(here) == "model" else os.path.join(os.getcwd(), "model")


def toml_str(s):
    return json.dumps(s, ensure_ascii=False)


def download(url, path):
    with urllib.request.urlopen(url) as r, open(path, "wb") as f:
        total = int(r.headers.get("Content-Length") or 0)
        done = 0
        while True:
            chunk = r.read(1 << 20)
            if not chunk:
                break
            f.write(chunk)
            done += len(chunk)
            if total > 1 << 20:
                print(f"\r  {os.path.basename(path)}: {done >> 20} / {total >> 20} MiB", end="", flush=True)
        if total > 1 << 20:
            print()


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("name", nargs="?", help="model name: the folder, and what queries call it")
    p.add_argument("--list", action="store_true", help="list the known models")
    p.add_argument("--dir", default=default_dir(), help="the model folder (default: %(default)s)")
    p.add_argument("--repo", help="Hugging Face repository (owner/name), for a model not in --list")
    p.add_argument("--revision", default="main", help="branch, tag or commit (default: main)")
    p.add_argument("--pooling", choices=("mean", "cls"))
    p.add_argument("--max-tokens", type=int)
    p.add_argument("--query-prefix")
    p.add_argument("--doc-prefix")
    p.add_argument("--matryoshka", choices=("none", "truncate", "layer-norm"),
                   help="whether the model makes shorter vectors too, and how (as its model card says)")
    p.add_argument("--force", action="store_true", help="replace an existing model of that name")
    a = p.parse_args()

    if a.list:
        for name, (repo, pooling, max_tokens, qp, _, mr) in KNOWN.items():
            print(f"{name:22} {repo}  ({pooling} pooling, {max_tokens} tokens{', query prefix' if qp else ''}"
                  f"{', Matryoshka' if mr else ''})")
        return 0
    if not a.name:
        p.error("name a model (see --list)")
    if not NAME.match(a.name):
        p.error("a model name is 1 to 64 of a-z, 0-9, '.', '_', '-', starting with a letter or digit")
    repo, pooling, max_tokens, qp, dp, mr = KNOWN.get(a.name, (None, "mean", None, "", "", None))
    repo = a.repo or repo
    if not repo:
        p.error(f"`{a.name}` is not a known model (see --list): give --repo owner/name")
    pooling = a.pooling or pooling
    max_tokens = a.max_tokens or max_tokens
    qp = qp if a.query_prefix is None else a.query_prefix
    dp = dp if a.doc_prefix is None else a.doc_prefix
    mr = mr if a.matryoshka is None else (None if a.matryoshka == "none" else a.matryoshka)

    target = os.path.join(a.dir, a.name)
    if os.path.exists(target) and not a.force:
        print(f"{target} exists; --force replaces it", file=sys.stderr)
        return 1
    endpoint = os.environ.get("HF_ENDPOINT", "https://huggingface.co").rstrip("/")
    os.makedirs(a.dir, exist_ok=True)
    work = tempfile.mkdtemp(prefix=f".{a.name}-", dir=a.dir)
    try:
        for f in FILES:
            url = f"{endpoint}/{repo}/resolve/{a.revision}/{f}"
            print(f"fetching {url}")
            download(url, os.path.join(work, f))
            if f == "config.json":
                # Before the weights: a model graphbuff cannot run is not downloaded.
                with open(os.path.join(work, f), encoding="utf-8") as c:
                    arch, dims, limit = model_shape(json.load(c))
        max_tokens = min(max_tokens or limit, limit)
        with open(os.path.join(work, "model.toml"), "w", encoding="utf-8") as f:
            f.write(f"# {repo} (revision {a.revision}), fetched by fetch-model.py\n"
                    f"architecture = \"{arch}\"\ndimensions = {dims}\nmax_tokens = {max_tokens}\n"
                    f"pooling = \"{pooling}\"\nnormalize = true\n"
                    f"query_prefix = {toml_str(qp)}\ndoc_prefix = {toml_str(dp)}\n"
                    + (f"matryoshka = \"{mr}\"\n" if mr else ""))
        if os.path.exists(target):
            shutil.rmtree(target)
        os.replace(work, target)
    except ValueError as e:
        shutil.rmtree(work, ignore_errors=True)
        print(f"cannot use {repo}: {e}", file=sys.stderr)
        return 1
    except urllib.error.URLError as e:
        shutil.rmtree(work, ignore_errors=True)
        print(f"cannot fetch {repo}: {e} (is the repository public, with {', '.join(FILES)}?)", file=sys.stderr)
        return 1
    except Exception:
        shutil.rmtree(work, ignore_errors=True)
        raise
    print(f"{a.name}: {arch}, {dims} dimensions, {pooling} pooling, in {target}")
    print("restart graphbuff-server to use it: RETURN vector.embed('hello', '%s')" % a.name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
