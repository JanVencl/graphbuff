"""Filtered vector search over Bolt: CREATE VECTOR INDEX … WITH, and MATCH … SEARCH."""

import pytest
from neo4j import exceptions as ex
from neo4j.time import Date

SEARCH = """
MATCH (d:Doc)
SEARCH d IN (
  VECTOR INDEX docs FOR $q
  WHERE d.tenant IN $tenants AND d.day >= $since
  LIMIT $k
) SCORE AS score
RETURN d.k AS k, score
"""


@pytest.fixture
def docs(session):
    session.run("DROP INDEX docs IF EXISTS").consume()
    session.run(
        "CREATE VECTOR INDEX docs FOR (d:Doc) ON d.e WITH [d.tenant, d.day] "
        "OPTIONS {indexConfig: {`vector.dimensions`: 2, `vector.similarity_function`: 'cosine'}}"
    ).consume()
    rows = [{"k": k, "e": [100.0 - k, float(k)], "tenant": f"t{k % 4}", "day": Date(2024, 1 + k % 12, 1)}
            for k in range(100)]
    session.run("UNWIND $rows AS r CREATE (d:Doc) SET d = r", rows=rows).consume()
    yield session
    session.run("DROP INDEX docs IF EXISTS").consume()


def expected(tenants, since, k):
    return [i for i in range(100) if f"t{i % 4}" in tenants and Date(2024, 1 + i % 12, 1) >= since][:k]


def test_parameters_of_every_kind(docs):
    for tenants, since, k in [(["t1"], Date(2024, 1, 1), 5), (["t0", "t3"], Date(2024, 7, 1), 10),
                              ([], Date(2024, 1, 1), 5), (["t2"], Date(2025, 1, 1), 5), (["t2"], Date(2024, 1, 1), 1000)]:
        records = list(docs.run(SEARCH, q=[1, 0], tenants=tenants, since=since, k=k))
        assert [r["k"] for r in records] == expected(tenants, since, k)
        scores = [r["score"] for r in records]
        assert all(isinstance(s, float) and 0 <= s <= 1 for s in scores)
        assert scores == sorted(scores, reverse=True)


def test_nodes_and_plan(docs):
    record = docs.run("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [0, 1] LIMIT 1) RETURN d").single()
    assert record["d"].labels == {"Doc"} and record["d"]["k"] == 99
    plan = docs.run("EXPLAIN " + SEARCH, q=[1, 0], tenants=["t1"], since=Date(2024, 1, 1), k=3).consume().plan

    def operators(p):
        return [p["operatorType"] + str(p.get("args", ""))] + [o for c in p.get("children", []) for o in operators(c)]

    assert any("VectorSearch" in o for o in operators(plan))


def test_inside_a_transaction(docs):
    with docs.begin_transaction() as tx:
        tx.run("CREATE (:Doc {k: -1, e: [1, 0], tenant: 'new', day: date('2024-05-01')})").consume()
        got = [r["k"] for r in tx.run(SEARCH, q=[1, 0], tenants=["new"], since=Date(2024, 1, 1), k=3)]
        assert got == [-1]
        tx.rollback()
    assert list(docs.run(SEARCH, q=[1, 0], tenants=["new"], since=Date(2024, 1, 1), k=3)) == []


def test_errors(docs):
    with pytest.raises(ex.ClientError, match="not a filter property"):
        docs.run("MATCH (d:Doc) SEARCH d IN (VECTOR INDEX docs FOR [1, 0] WHERE d.k = 1 LIMIT 1) RETURN d").consume()
    with pytest.raises(ex.ClientError, match="dimensions"):
        docs.run(SEARCH, q=[1], tenants=["t1"], since=Date(2024, 1, 1), k=1).consume()
