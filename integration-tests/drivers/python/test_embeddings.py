"""Embeddings end to end, over HTTP and Bolt: texts embedded in the server with
vector.embedBatch, stored, indexed with CREATE VECTOR INDEX, and searched with
db.index.vector.queryNodes and vector.embed. The model is the tiny random BERT test
fixture (crates/embed/tests/fixtures/tiny-bert): its vectors mean nothing, but a text
is always closest to itself.

Needs a server built with the `embed` feature (cargo build -p graphbuff-server
--features embed); skipped otherwise.
"""

import os
import shutil
import sys
import tempfile

import pytest
from neo4j import GraphDatabase

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "python"))
from graphbuff_server import QueryError, ROOT, Server, free_port  # noqa: E402

FIXTURE = os.path.join(ROOT, "crates", "embed", "tests", "fixtures", "tiny-bert")
TOKEN = "embed-secret"
DOCS = [
    "graph databases store nodes and relationships",
    "vector indexes find similar embeddings quickly",
    "the quick brown fox jumps over the lazy dog",
    "cypher queries match patterns in the graph",
]


@pytest.fixture(scope="module")
def embed_server():
    models = tempfile.mkdtemp(prefix="graphbuff-models-")
    model = os.path.join(models, "tiny")
    os.makedirs(model)
    for f in ("model.toml", "config.json", "tokenizer.json", "model.safetensors"):
        shutil.copy(os.path.join(FIXTURE, f), model)
    bolt_port = free_port()
    srv = Server({"server.bolt": f"127.0.0.1:{bolt_port}", "security.token": TOKEN,
                  "embedding.model_dir": models, "embedding.threads": 1})
    try:
        srv.start()
    except RuntimeError as e:
        shutil.rmtree(models, ignore_errors=True)
        pytest.exit(str(e), returncode=2)
    try:
        srv.query("RETURN vector.embed('probe') AS v")
    except QueryError as e:
        srv.stop()
        shutil.rmtree(models, ignore_errors=True)
        pytest.skip(f"the server has no embedding models ({e.message}); build it with --features embed")
    yield srv, f"bolt://127.0.0.1:{bolt_port}"
    srv.stop()
    shutil.rmtree(models, ignore_errors=True)


def load_and_index(srv):
    srv.query("MATCH (n) DETACH DELETE n")
    srv.query("DROP INDEX docs IF EXISTS")
    srv.query(
        "UNWIND $docs AS text CREATE (:Doc {text: text}) WITH count(*) AS n "
        "MATCH (d:Doc) WITH collect(d) AS docs "
        "CALL vector.embedBatch([d IN docs | d.text]) YIELD index, vector "
        "WITH docs[index] AS d, vector SET d.embedding = vector",
        {"docs": DOCS},
    )
    srv.query("CREATE VECTOR INDEX docs FOR (d:Doc) ON d.embedding "
              "OPTIONS {indexConfig: {`vector.dimensions`: 32, `vector.similarity_function`: 'cosine'}}")
    srv.query("CALL db.awaitIndexes()")


SEARCH = ("CALL db.index.vector.queryNodes('docs', 2, vector.embed($q)) "
          "YIELD node, score RETURN node.text AS text, score")


def test_over_http(embed_server):
    srv, _ = embed_server
    load_and_index(srv)
    models = srv.query("CALL vector.models() YIELD name, dimensions, loaded RETURN name, dimensions, loaded")
    assert models["rows"] == [["tiny", 32, True]]
    for doc in DOCS:
        rows = srv.query(SEARCH, {"q": doc})["rows"]
        assert rows[0][0] == doc
        assert rows[0][1] == pytest.approx(1.0, abs=1e-5)
        assert len(rows) == 2 and rows[1][1] < rows[0][1]
    stored = srv.query("MATCH (d:Doc) RETURN size(d.embedding) AS n LIMIT 1")["rows"]
    assert stored == [[32]]


def test_over_bolt(embed_server):
    srv, bolt = embed_server
    load_and_index(srv)
    with GraphDatabase.driver(bolt, auth=("neo4j", TOKEN)) as d:
        v, _, _ = d.execute_query("RETURN vector.embed($t) AS v", t=DOCS[0])
        vector = v[0]["v"]
        assert len(vector) == 32 and all(isinstance(x, float) for x in vector)
        assert sum(x * x for x in vector) == pytest.approx(1.0, abs=1e-5)
        for doc in DOCS:
            records, _, _ = d.execute_query(SEARCH, q=doc)
            assert records[0]["text"] == doc
            assert records[0]["score"] == pytest.approx(1.0, abs=1e-5)
        # Writing through Bolt: the same vectors as over HTTP.
        d.execute_query("CREATE (:Doc {text: $t, embedding: vector.embed($t)})", t="written over bolt")
        records, _, _ = d.execute_query(SEARCH, q="written over bolt")
        assert records[0]["text"] == "written over bolt"
    http = srv.query("RETURN vector.embed($t) AS v", {"t": DOCS[0]})["rows"][0][0]
    assert http == pytest.approx(vector, abs=1e-6)
