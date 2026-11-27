# tests/fixtures/tiny-m3: tiny-xlmr with random bge-m3 heads (sparse_linear, colbert_linear),
# and its reference outputs as BGE-M3 computes them (FlagEmbedding's M3 embedder):
#   python scripts/make_m3_fixture.py
# The weights go through fetch-model.py's PyTorch conversion (pytorch_model.bin and the
# heads' .pt files, as bge-m3 publishes them, to one model.safetensors). Needs torch,
# transformers and safetensors.
#
#   dense:  the first token's hidden state ([CLS], <s>), L2-normalized
#   sparse: relu(sparse_linear(h)) per token; special tokens (cls, eos, pad, unk) and
#           weights <= 0 left out; a repeated token keeps its largest weight
#   multi:  colbert_linear(h[1:]) per token after the first, up to the last, each
#           L2-normalized
import importlib.util, json, os, shutil, sys, tempfile, torch
from transformers import PreTrainedTokenizerFast, XLMRobertaModel

HERE = os.path.dirname(os.path.abspath(__file__))
SRC, OUT = "tests/fixtures/tiny-xlmr", "tests/fixtures/tiny-m3"
MAX_TOKENS = 24
texts = [
    "graph databases store nodes",
    "a",
    "",
    "Vector indexes find SIMILAR embeddings",
    "Příliš žluťoučký kůň",
    "graph graph graph nodes",
    " ".join(["graph nodes and vectors"] * 10),  # longer than 24 tokens: truncated
]

spec = importlib.util.spec_from_file_location("fetch_model", os.path.join(HERE, "..", "..", "..", "scripts", "fetch-model.py"))
fetch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetch)

model = XLMRobertaModel.from_pretrained(SRC, add_pooling_layer=False).eval()
h_size = model.config.hidden_size
torch.manual_seed(17)
sparse = torch.nn.Linear(h_size, 1)
colbert = torch.nn.Linear(h_size, h_size)
with torch.no_grad():
    sparse.weight.mul_(4.0)  # weights both above and below 0

work = tempfile.mkdtemp()
torch.save(model.state_dict(), os.path.join(work, "pytorch_model.bin"))
torch.save(sparse.state_dict(), os.path.join(work, "sparse_linear.pt"))
torch.save(colbert.state_dict(), os.path.join(work, "colbert_linear.pt"))
heads = fetch.convert_pytorch(work, "pytorch_model.bin", {"sparse_linear.pt": "sparse_linear", "colbert_linear.pt": "colbert_linear"})
os.makedirs(OUT, exist_ok=True)
shutil.copy(os.path.join(work, "model.safetensors"), OUT)
for f in ["config.json", "tokenizer.json"]:
    shutil.copy(os.path.join(SRC, f), OUT)
with open(os.path.join(OUT, "model.toml"), "w") as f:
    f.write("# tiny-xlmr with random bge-m3 heads (scripts/make_m3_fixture.py); not a useful model.\n"
            f'architecture = "xlm-roberta"\ndimensions = {h_size}\nmax_tokens = {MAX_TOKENS}\npooling = "cls"\nnormalize = true\n'
            'query_prefix = "query: "\ndoc_prefix = ""\n'
            + ("sparse = true\n" if "sparse_linear" in heads else "")
            + ("multi_vector = true\n" if "colbert_linear" in heads else ""))

tok = PreTrainedTokenizerFast(tokenizer_file=os.path.join(OUT, "tokenizer.json"), bos_token="<s>", eos_token="</s>",
                              unk_token="<unk>", pad_token="<pad>", cls_token="<s>", sep_token="</s>")
unused = {tok.cls_token_id, tok.eos_token_id, tok.pad_token_id, tok.unk_token_id}


def run(ts):
    enc = tok(ts, padding=True, truncation=True, max_length=MAX_TOKENS, return_tensors="pt", return_token_type_ids=False)
    with torch.no_grad():
        h = model(**enc).last_hidden_state
        dense = torch.nn.functional.normalize(h[:, 0], dim=-1)
        weights = torch.relu(sparse(h)).squeeze(-1)
        multi = colbert(h[:, 1:]) * enc["attention_mask"][:, 1:][:, :, None].float()
        multi = torch.nn.functional.normalize(multi, dim=-1)
    out = {"ids": [], "dense": dense.tolist(), "sparse": [], "multi": []}
    for r in range(len(ts)):
        n = int(enc["attention_mask"][r].sum())
        ids = [int(i) for i in enc["input_ids"][r][:n]]
        best = {}
        for i, w in zip(ids, weights[r][:n].tolist()):
            if i not in unused and w > 0 and w > best.get(i, 0):
                best[i] = w
        out["ids"].append(ids)
        out["sparse"].append({tok.convert_ids_to_tokens(i): w for i, w in best.items()})
        out["multi"].append(multi[r][: n - 1].tolist())
    return out


doc = run(texts)
query = run(["query: " + t for t in texts])
json.dump({"texts": texts, **doc, "query_sparse": query["sparse"], "query_multi": query["multi"]},
          open(os.path.join(OUT, "reference.json"), "w"), indent=0)
print("ok", OUT, [len(i) for i in doc["ids"]], "tokens;", [len(s) for s in doc["sparse"]], "weighted")
