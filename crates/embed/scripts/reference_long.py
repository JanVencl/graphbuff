# Reference vectors of long texts for tiny-nomic with rotary scaling, for tests/golden.rs:
#   python scripts/reference_long.py NOMIC_CODE
# writes tests/fixtures/tiny-nomic/reference-long.json. NOMIC_CODE is a folder with
# modeling_hf_nomic_bert.py and configuration_hf_nomic_bert.py (from
# huggingface.co/nomic-ai/nomic-bert-2048). Needs torch, transformers, tokenizers, einops.
#
# Each text runs alone in a fresh model: Nomic's rotary module keeps its cos/sin tables
# and its last scaled base between calls, so a model reused across texts would give each
# text the tables of the longest one before it.
import importlib, json, os, shutil, sys, tempfile, torch
from safetensors.torch import load_file
from transformers import PreTrainedTokenizerFast

FIXTURE = "tests/fixtures/tiny-nomic"
SCALING = 2.0
MAX_TOKENS = 64  # the fixture's n_positions
texts = [" ".join(["graph nodes and vectors"] * n) for n in (6, 10, 20)] + ["graph databases store nodes"]

pkg = tempfile.mkdtemp()
shutil.copytree(sys.argv[1], os.path.join(pkg, "nomic_code"), ignore=shutil.ignore_patterns("*.safetensors", "*.bin"))
open(os.path.join(pkg, "nomic_code", "__init__.py"), "w").close()
sys.path.insert(0, pkg)
nomic = importlib.import_module("nomic_code.modeling_hf_nomic_bert")

tok = PreTrainedTokenizerFast(tokenizer_file=os.path.join(FIXTURE, "tokenizer.json"), pad_token="[PAD]")
weights = load_file(os.path.join(FIXTURE, "model.safetensors"))
config = nomic.NomicBertConfig.from_pretrained(FIXTURE)
config.rotary_scaling_factor = SCALING
ids, vectors = [], []
for t in texts:
    model = nomic.NomicBertModel(config, add_pooling_layer=False)
    model.load_state_dict(weights, strict=True)
    model.eval()
    enc = tok([t], truncation=True, max_length=MAX_TOKENS, return_tensors="pt")
    with torch.no_grad():
        h = model(input_ids=enc["input_ids"], attention_mask=enc["attention_mask"],
                  token_type_ids=torch.zeros_like(enc["input_ids"])).last_hidden_state
    v = torch.nn.functional.normalize(h.mean(1), dim=1)[0]
    ids.append([int(i) for i in enc["input_ids"][0]])
    vectors.append(v.tolist())
json.dump({"rotary_scaling": SCALING, "texts": texts, "ids": ids, "mean": vectors},
          open(os.path.join(FIXTURE, "reference-long.json"), "w"), indent=0)
print(len(texts), "texts,", [len(i) for i in ids], "tokens")
