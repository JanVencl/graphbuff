# Builds a tiny random model fixture in tests/fixtures/ and its reference vectors. Run from
# crates/embed:
#   python scripts/make_fixture.py bert          # tests/fixtures/tiny-bert (HF transformers)
#   python scripts/make_fixture.py xlm-roberta   # tests/fixtures/tiny-xlmr (HF transformers)
#   python scripts/make_fixture.py nomic-bert NOMIC_CODE
#                                                # tests/fixtures/tiny-nomic, with Nomic's own
#                                                # model code: NOMIC_CODE is a folder with
#                                                # modeling_hf_nomic_bert.py and
#                                                # configuration_hf_nomic_bert.py (from
#                                                # huggingface.co/nomic-ai/nomic-bert-2048)
# Needs torch, transformers and tokenizers (nomic-bert also einops). The output is
# committed; rerun only to change it.
import importlib, json, os, shutil, sys, tempfile, torch
from tokenizers import SentencePieceUnigramTokenizer, Tokenizer, models, normalizers, pre_tokenizers, processors, trainers
from transformers import BertConfig, BertModel, PreTrainedTokenizerFast, XLMRobertaConfig, XLMRobertaModel

corpus = [
    "graph databases store nodes and relationships",
    "vector indexes find similar embeddings quickly",
    "the quick brown fox jumps over the lazy dog",
    "cypher queries match patterns in the graph",
    "a model turns text into vectors of numbers",
    "Příliš žluťoučký kůň úpěl ďábelské ódy",
] * 20
texts = [
    "graph databases store nodes",
    "a",
    "",
    "Vector indexes find SIMILAR embeddings",
    "Příliš žluťoučký kůň",
    "unseen words like zebra xylophone",
    " ".join(["graph nodes and vectors"] * 10),  # longer than 24 tokens: truncated
]
MAX_TOKENS = 24


def wordpiece():
    tok = Tokenizer(models.WordPiece(unk_token="[UNK]"))
    tok.normalizer = normalizers.BertNormalizer(lowercase=True, strip_accents=None)
    tok.pre_tokenizer = pre_tokenizers.BertPreTokenizer()
    tok.train_from_iterator(corpus, trainers.WordPieceTrainer(vocab_size=300, special_tokens=["[PAD]", "[UNK]", "[CLS]", "[SEP]", "[MASK]"]))
    cls, sep = tok.token_to_id("[CLS]"), tok.token_to_id("[SEP]")
    tok.post_processor = processors.TemplateProcessing(single="[CLS] $A [SEP]", special_tokens=[("[CLS]", cls), ("[SEP]", sep)])
    hf = PreTrainedTokenizerFast(tokenizer_object=tok, unk_token="[UNK]", pad_token="[PAD]", cls_token="[CLS]", sep_token="[SEP]")
    return tok, hf


def write(out, model, hf, architecture, seq_len_args):
    """Reference vectors of `texts` (mean, CLS, with a query prefix) and model.toml."""
    def run(ts):
        enc = hf(ts, padding=True, truncation=True, max_length=MAX_TOKENS, return_tensors="pt", **seq_len_args)
        with torch.no_grad():
            h = model(**enc).last_hidden_state
        m = enc["attention_mask"].unsqueeze(-1).float()
        mean = torch.nn.functional.normalize((h * m).sum(1) / m.sum(1).clamp(min=1e-9), dim=1)
        ids = [[int(t) for t, a in zip(r, am) if a] for r, am in zip(enc["input_ids"], enc["attention_mask"])]
        return ids, mean.tolist(), h[:, 0].tolist()
    ids, mean, cls_raw = run(texts)
    qids, qmean, _ = run(["query: " + t for t in texts])
    json.dump({"texts": texts, "ids": ids, "mean": mean, "cls": cls_raw, "query_prefix": "query: ", "query_ids": qids, "query_mean": qmean},
              open(os.path.join(out, "reference.json"), "w"), indent=0)
    with open(os.path.join(out, "model.toml"), "w") as f:
        f.write(f'# Tiny random {architecture} for tests (scripts/make_fixture.py); not a useful model.\n'
                f'architecture = "{architecture}"\ndimensions = 32\nmax_tokens = {MAX_TOKENS}\npooling = "mean"\nnormalize = true\n'
                'query_prefix = "query: "\ndoc_prefix = ""\n')


def shake_norms(model):
    """LayerNorms start at weight 1, bias 0; random ones test that they are applied."""
    with torch.no_grad():
        for m in model.modules():
            if isinstance(m, torch.nn.LayerNorm):
                m.weight.add_(torch.randn_like(m.weight) * 0.2)
                m.bias.add_(torch.randn_like(m.bias) * 0.2)


def bert():
    out = "tests/fixtures/tiny-bert"
    tok, hf = wordpiece()
    tok.save(os.path.join(out, "tokenizer.json"))
    torch.manual_seed(7)
    cfg = BertConfig(vocab_size=tok.get_vocab_size(), hidden_size=32, num_hidden_layers=2, num_attention_heads=4,
                     intermediate_size=64, max_position_embeddings=MAX_TOKENS, initializer_range=0.2, pad_token_id=0)
    model = BertModel(cfg, add_pooling_layer=False).eval()
    model.save_pretrained(out, safe_serialization=True)
    write(out, model, hf, "bert", {})


def xlm_roberta():
    out = "tests/fixtures/tiny-xlmr"
    os.makedirs(out, exist_ok=True)
    tok = SentencePieceUnigramTokenizer()
    tok.train_from_iterator(corpus, vocab_size=200, special_tokens=["<s>", "<pad>", "</s>", "<unk>"], unk_token="<unk>")
    tok.post_processor = processors.TemplateProcessing(single="<s> $A </s>", special_tokens=[("<s>", 0), ("</s>", 2)])
    tok.save(os.path.join(out, "tokenizer.json"))
    hf = PreTrainedTokenizerFast(tokenizer_file=os.path.join(out, "tokenizer.json"), bos_token="<s>", eos_token="</s>",
                                 unk_token="<unk>", pad_token="<pad>", cls_token="<s>", sep_token="</s>")
    torch.manual_seed(11)
    # Positions start at pad_token_id + 1 = 2: 24 tokens need 26 rows.
    cfg = XLMRobertaConfig(vocab_size=tok.get_vocab_size(), hidden_size=32, num_hidden_layers=2, num_attention_heads=4,
                           intermediate_size=64, max_position_embeddings=MAX_TOKENS + 2, type_vocab_size=1,
                           initializer_range=0.2, pad_token_id=1, bos_token_id=0, eos_token_id=2)
    model = XLMRobertaModel(cfg, add_pooling_layer=False).eval()
    shake_norms(model)
    model.save_pretrained(out, safe_serialization=True)
    write(out, model, hf, "xlm-roberta", {"return_token_type_ids": False})


def nomic_bert(code):
    out = "tests/fixtures/tiny-nomic"
    os.makedirs(out, exist_ok=True)
    pkg = tempfile.mkdtemp()
    shutil.copytree(code, os.path.join(pkg, "nomic_code"), ignore=shutil.ignore_patterns("*.safetensors", "*.bin"))
    open(os.path.join(pkg, "nomic_code", "__init__.py"), "w").close()
    sys.path.insert(0, pkg)
    nomic = importlib.import_module("nomic_code.modeling_hf_nomic_bert")
    tok, hf = wordpiece()
    tok.save(os.path.join(out, "tokenizer.json"))
    torch.manual_seed(13)
    cfg = nomic.NomicBertConfig(vocab_size=tok.get_vocab_size(), n_embd=32, n_layer=2, n_head=4, n_inner=64, n_positions=64,
                                max_trained_positions=MAX_TOKENS, activation_function="swiglu", rotary_emb_fraction=1.0,
                                rotary_emb_base=1000, qkv_proj_bias=False, mlp_fc1_bias=False, mlp_fc2_bias=False,
                                type_vocab_size=2, layer_norm_epsilon=1e-12, pad_vocab_size_multiple=64,
                                initializer_range=0.2, pad_token_id=0, use_flash_attn=False, fused_bias_fc=False,
                                fused_dropout_add_ln=False, resid_pdrop=0.0, embd_pdrop=0.0, attn_pdrop=0.0)
    model = nomic.NomicBertModel(cfg, add_pooling_layer=False).eval()
    shake_norms(model)
    model.save_pretrained(out, safe_serialization=True)
    write(out, model, hf, "nomic-bert", {})


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "bert"
    {"bert": bert, "xlm-roberta": xlm_roberta, "nomic-bert": lambda: nomic_bert(sys.argv[2])}[which]()
    print("ok", which)
