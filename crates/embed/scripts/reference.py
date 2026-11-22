# Reference vectors of a real model, for the ignored golden test (tests/golden.rs):
#   python scripts/reference.py path/to/model/minilm
# writes path/to/model/minilm/reference.json. Mean pooling or CLS and normalization as the
# folder's model.toml says. Needs torch and transformers.
import json, os, sys, torch
from transformers import AutoModel, AutoTokenizer

d = sys.argv[1]
manifest = dict(
    (k.strip(), v.split("#")[0].strip().strip("\"'"))
    for k, v in (l.split("=", 1) for l in open(os.path.join(d, "model.toml"), encoding="utf-8") if "=" in l and not l.lstrip().startswith("#"))
)
texts = [
    "The quick brown fox jumps over the lazy dog.",
    "Graph databases store nodes and relationships.",
    "a",
    "",
    "Příliš žluťoučký kůň úpěl ďábelské ódy.",
    "MATCH (n:Person)-[:KNOWS]->(m) RETURN m.name LIMIT 10",
    "🚀 emoji & punctuation!!! ??? ...",
    " ".join(["graph node edge vector index query text model token layer"] * 70),  # truncated
]
prefix = manifest.get("doc_prefix", "")
tok = AutoTokenizer.from_pretrained(d)
model = AutoModel.from_pretrained(d).eval()
max_tokens = int(manifest.get("max_tokens", model.config.max_position_embeddings))
enc = tok([prefix + t for t in texts], padding=True, truncation=True, max_length=max_tokens, return_tensors="pt")
with torch.no_grad():
    h = model(**enc).last_hidden_state
m = enc["attention_mask"].unsqueeze(-1).float()
v = h[:, 0] if manifest.get("pooling", "mean") == "cls" else (h * m).sum(1) / m.sum(1).clamp(min=1e-9)
if manifest.get("normalize", "true") == "true":
    v = torch.nn.functional.normalize(v, dim=1)
ids = [[int(t) for t, a in zip(r, am) if a] for r, am in zip(enc["input_ids"], enc["attention_mask"])]
json.dump({"texts": texts, "ids": ids, "vectors": v.tolist()}, open(os.path.join(d, "reference.json"), "w"))
print(len(texts), "texts,", v.shape[1], "dimensions")
