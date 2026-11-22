# Builds the tiny random BERT fixture in tests/fixtures/tiny-bert/ and its reference
# vectors (HF transformers). Run from crates/embed: python scripts/make_fixture.py
# Needs torch, transformers and tokenizers. The output is committed; rerun only to change it.
import json, os, torch
from tokenizers import Tokenizer, models, normalizers, pre_tokenizers, processors, trainers
from transformers import BertConfig, BertModel, PreTrainedTokenizerFast

out = "tests/fixtures/tiny-bert"
corpus = [
    "graph databases store nodes and relationships",
    "vector indexes find similar embeddings quickly",
    "the quick brown fox jumps over the lazy dog",
    "cypher queries match patterns in the graph",
    "a model turns text into vectors of numbers",
    "Příliš žluťoučký kůň úpěl ďábelské ódy",
] * 20
tok = Tokenizer(models.WordPiece(unk_token="[UNK]"))
tok.normalizer = normalizers.BertNormalizer(lowercase=True, strip_accents=None)
tok.pre_tokenizer = pre_tokenizers.BertPreTokenizer()
tok.train_from_iterator(corpus, trainers.WordPieceTrainer(vocab_size=300, special_tokens=["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"]))
cls, sep = tok.token_to_id("[CLS]"), tok.token_to_id("[SEP]")
tok.post_processor = processors.TemplateProcessing(single="[CLS] $A [SEP]", special_tokens=[("[CLS]", cls), ("[SEP]", sep)])
tok.save(os.path.join(out, "tokenizer.json"))

torch.manual_seed(7)
cfg = BertConfig(vocab_size=tok.get_vocab_size(), hidden_size=32, num_hidden_layers=2, num_attention_heads=4,
                 intermediate_size=64, max_position_embeddings=24, initializer_range=0.2, pad_token_id=0)
model = BertModel(cfg, add_pooling_layer=False).eval()
model.save_pretrained(out, safe_serialization=True)

hf = PreTrainedTokenizerFast(tokenizer_object=tok, unk_token="[UNK]", pad_token="[PAD]", cls_token="[CLS]", sep_token="[SEP]")
texts = [
    "graph databases store nodes",
    "a",
    "",
    "Vector indexes find SIMILAR embeddings",
    "Příliš žluťoučký kůň",
    "unseen words like zebra xylophone",
    " ".join(["graph nodes and vectors"] * 10),  # longer than 24 tokens: truncated
]
prefixed = ["query: " + t for t in texts]
def run(ts):
    enc = hf(ts, padding=True, truncation=True, max_length=cfg.max_position_embeddings, return_tensors="pt")
    with torch.no_grad():
        h = model(**enc).last_hidden_state
    m = enc["attention_mask"].unsqueeze(-1).float()
    mean = torch.nn.functional.normalize((h * m).sum(1) / m.sum(1).clamp(min=1e-9), dim=1)
    ids = [[int(t) for t, a in zip(r, am) if a] for r, am in zip(enc["input_ids"], enc["attention_mask"])]
    return ids, mean.tolist(), h[:, 0].tolist()
ids, mean, cls_raw = run(texts)
qids, qmean, _ = run(prefixed)
json.dump({"texts": texts, "ids": ids, "mean": mean, "cls": cls_raw, "query_prefix": "query: ", "query_ids": qids, "query_mean": qmean},
          open(os.path.join(out, "reference.json"), "w"), indent=0)
with open(os.path.join(out, "model.toml"), "w") as f:
    f.write('# Tiny random BERT for tests (scripts/make_fixture.py); not a useful model.\n'
            'architecture = "bert"\ndimensions = 32\nmax_tokens = 24\npooling = "mean"\nnormalize = true\n'
            'query_prefix = "query: "\ndoc_prefix = ""\n')
print("ok", cfg.vocab_size, sum(p.numel() for p in model.parameters()), "params")
