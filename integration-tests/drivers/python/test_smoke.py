"""Smoke suite: the official Neo4j Python driver against graphbuff-server over Bolt."""

import datetime
import threading

import pytest
from neo4j import GraphDatabase, Query, RoutingControl, Record
from neo4j import exceptions as ex
from neo4j.graph import Node, Path, Relationship
from neo4j.spatial import CartesianPoint, WGS84Point
from neo4j.time import Date, DateTime, Duration, Time


# --- connection -------------------------------------------------------------

def test_verify_connectivity_and_server_info(driver):
    driver.verify_connectivity()
    info = driver.get_server_info()
    assert info.agent.startswith("Neo4j/")
    assert info.protocol_version >= (4, 4)


def test_routing_scheme(server):
    with GraphDatabase.driver(server["neo4j"], auth=server["auth"]) as d:
        d.verify_connectivity()
        records, _, _ = d.execute_query("RETURN 1 AS x", routing_=RoutingControl.READ)
        assert records[0]["x"] == 1
        records, _, _ = d.execute_query("RETURN 2 AS x", routing_=RoutingControl.WRITE)
        assert records[0]["x"] == 2


def test_wrong_password(server):
    with GraphDatabase.driver(server["bolt"], auth=("neo4j", "wrong")) as d:
        with pytest.raises(ex.AuthError):
            d.verify_connectivity()


def test_bearer_auth(server):
    from neo4j import bearer_auth
    with GraphDatabase.driver(server["bolt"], auth=bearer_auth(server["auth"][1])) as d:
        d.verify_connectivity()


# --- values -----------------------------------------------------------------

def test_scalar_values(session):
    rec = session.run(
        "RETURN 1 AS i, -9007199254740993 AS big, 1.5 AS f, 'héllo' AS s, true AS b,"
        " null AS n, [1, 'a', null] AS l, {k: [1, {x: 2}]} AS m").single()
    assert rec["i"] == 1
    assert rec["big"] == -9007199254740993
    assert rec["f"] == 1.5
    assert rec["s"] == "héllo"
    assert rec["b"] is True
    assert rec["n"] is None
    assert rec["l"] == [1, "a", None]
    assert rec["m"] == {"k": [1, {"x": 2}]}
    assert rec.keys() == ["i", "big", "f", "s", "b", "n", "l", "m"]


def test_parameters_round_trip(session):
    params = {
        "i": 2 ** 62, "f": -0.25, "s": "x" * 70000, "b": False, "n": None,
        "l": list(range(300)), "m": {"nested": {"deep": ["a", 1, 2.5]}},
        "empty_l": [], "empty_m": {},
    }
    rec = session.run("RETURN $i AS i, $f AS f, $s AS s, $b AS b, $n AS n, $l AS l,"
                      " $m AS m, $empty_l AS el, $empty_m AS em", params).single()
    assert rec["i"] == params["i"]
    assert rec["f"] == params["f"]
    assert rec["s"] == params["s"]
    assert rec["b"] is False and rec["n"] is None
    assert rec["l"] == params["l"]
    assert rec["m"] == params["m"]
    assert rec["el"] == [] and rec["em"] == {}


def test_temporal_values(session):
    rec = session.run(
        "RETURN date('2024-02-29') AS d, localtime('12:34:56.789') AS lt,"
        " time('12:00:00+02:00') AS t, localdatetime('2024-01-02T03:04:05') AS ldt,"
        " datetime('2024-01-02T03:04:05.123456789+01:00') AS dt,"
        " duration('P1Y2M3DT4H5M6.5S') AS dur").single()
    assert rec["d"] == Date(2024, 2, 29)
    assert rec["lt"] == Time(12, 34, 56, 789000000) and rec["lt"].tzinfo is None
    assert rec["t"] == Time(12, 0, 0, tzinfo=datetime.timezone(datetime.timedelta(hours=2)))
    assert rec["ldt"] == DateTime(2024, 1, 2, 3, 4, 5) and rec["ldt"].tzinfo is None
    dt = rec["dt"]
    assert isinstance(dt, DateTime)
    assert (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second) == (2024, 1, 2, 3, 4, 5)
    assert dt.nanosecond == 123456789
    assert dt.utcoffset() == datetime.timedelta(hours=1)
    assert rec["dur"] == Duration(years=1, months=2, days=3, hours=4, minutes=5, seconds=6.5)


def test_temporal_parameters(session):
    tz = datetime.timezone(datetime.timedelta(hours=-5))
    params = {
        "d": datetime.date(2001, 2, 3),
        "ldt": datetime.datetime(2001, 2, 3, 4, 5, 6),
        "dt": datetime.datetime(2001, 2, 3, 4, 5, 6, tzinfo=tz),
        "dt_utc": datetime.datetime(2001, 2, 3, 4, 5, 6, tzinfo=datetime.timezone.utc),
        "lt": datetime.time(7, 8, 9),
        "dur": datetime.timedelta(days=2, seconds=30),
    }
    rec = session.run(
        "RETURN toString($d) AS d, toString($ldt) AS ldt, toString($dt) AS dt,"
        " toString($dt_utc) AS dt_utc, toString($lt) AS lt, toString($dur) AS dur,"
        " $dt.epochSeconds AS epoch", params).single()
    assert rec["d"] == "2001-02-03"
    assert rec["ldt"] == "2001-02-03T04:05:06"
    assert rec["dt"] == "2001-02-03T04:05:06-05:00"
    assert rec["dt_utc"] == "2001-02-03T04:05:06Z"
    assert rec["lt"] == "07:08:09"
    assert rec["dur"] == "P2DT30S"
    assert rec["epoch"] == int(params["dt"].timestamp())
    # Round trip unchanged.
    rec = session.run("RETURN $dt AS dt, $d AS d", params).single()
    assert rec["dt"].to_native() == params["dt"]
    assert rec["d"].to_native() == params["d"]


def test_spatial_values(session):
    rec = session.run("RETURN point({x: 1.5, y: -2}) AS c, point({x: 1, y: 2, z: 3}) AS c3,"
                      " point({longitude: 14.42, latitude: 50.09}) AS g").single()
    assert isinstance(rec["c"], CartesianPoint) and tuple(rec["c"]) == (1.5, -2.0)
    assert tuple(rec["c3"]) == (1.0, 2.0, 3.0) and rec["c3"].srid == 9157
    assert isinstance(rec["g"], WGS84Point)
    assert (rec["g"].longitude, rec["g"].latitude) == (14.42, 50.09)
    rec = session.run("RETURN $p.x AS x, point.distance($p, point({x: 0, y: 0})) AS d",
                      p=CartesianPoint((3, 4))).single()
    assert rec["x"] == 3.0 and rec["d"] == 5.0


def test_graph_values(session):
    rec = session.run(
        "CREATE p = (a:Person:Admin {name: 'Ann', age: 30})-[r:KNOWS {since: 2020}]->"
        "(b:Person {name: 'Bob'}) RETURN a, r, b, p").single()
    a, r, b, p = rec["a"], rec["r"], rec["b"], rec["p"]
    assert isinstance(a, Node) and a.labels == {"Person", "Admin"}
    assert dict(a) == {"name": "Ann", "age": 30}
    assert isinstance(r, Relationship) and r.type == "KNOWS" and r["since"] == 2020
    assert r.start_node.element_id == a.element_id
    assert r.end_node.element_id == b.element_id
    assert isinstance(p, Path) and len(p) == 1
    assert [n["name"] for n in p.nodes] == ["Ann", "Bob"]
    # Element ids are stable and usable from Cypher.
    found = session.run("MATCH (n) WHERE elementId(n) = $id RETURN n.name AS name",
                        id=b.element_id).single()
    assert found["name"] == "Bob"


def test_reverse_relationship_in_path(session):
    session.run("CREATE (:A {n: 1})-[:R]->(:B {n: 2})").consume()
    p = session.run("MATCH p = (:B)<-[:R]-(:A) RETURN p").single()["p"]
    assert [n["n"] for n in p.nodes] == [2, 1]
    rel = p.relationships[0]
    assert rel.start_node["n"] == 1 and rel.end_node["n"] == 2


# --- results ----------------------------------------------------------------

def test_streaming_with_small_fetch_size(driver):
    with driver.session(fetch_size=7) as s:
        result = s.run("UNWIND range(1, 1000) AS x RETURN x")
        assert [r["x"] for r in result] == list(range(1, 1001))


def test_partially_consumed_results(driver):
    with driver.session(fetch_size=10) as s:
        first = s.run("UNWIND range(1, 100) AS x RETURN x")
        assert next(iter(first))["x"] == 1
        # A new query in the same session discards the rest of the first.
        assert s.run("RETURN 42 AS x").single()["x"] == 42


def test_summary_counters(session):
    summary = session.run(
        "CREATE (a:L {p: 1})-[:T]->(b:L), (c:M) SET c.q = 2 RETURN a").consume()
    c = summary.counters
    assert c.nodes_created == 3
    assert c.relationships_created == 1
    assert c.labels_added == 3
    assert c.properties_set == 2
    assert c.contains_updates
    assert summary.query_type == "rw"
    summary = session.run("MATCH (n) DETACH DELETE n").consume()
    assert summary.counters.nodes_deleted == 3
    assert summary.counters.relationships_deleted == 1
    assert session.run("RETURN 1").consume().query_type == "r"


def test_schema_counters(session):
    s = session.run("CREATE INDEX smoke_idx IF NOT EXISTS FOR (n:Smoke) ON (n.k)").consume()
    assert s.counters.indexes_added == 1
    assert s.query_type == "s"
    s = session.run("DROP INDEX smoke_idx").consume()
    assert s.counters.indexes_removed == 1


def test_explain_and_profile(session):
    s = session.run("EXPLAIN MATCH (n) RETURN n").consume()
    assert s.plan is not None and "operatorType" in s.plan
    session.run("CREATE (:X), (:X)").consume()
    s = session.run("PROFILE MATCH (n:X) RETURN n").consume()
    assert s.profile is not None and s.profile["rows"] == 2


def test_execute_query_api(driver):
    driver.execute_query("CREATE (:Q {v: $v})", v=7)
    records, summary, keys = driver.execute_query("MATCH (q:Q) RETURN q.v AS v")
    assert keys == ["v"] and [r["v"] for r in records] == [7]
    assert summary.database
    driver.execute_query("MATCH (q:Q) DELETE q")


def test_record_helpers(session):
    rec = session.run("RETURN 1 AS a, 2 AS b").single()
    assert isinstance(rec, Record)
    assert rec.data() == {"a": 1, "b": 2}
    assert session.run("UNWIND [1, 2, 3] AS x RETURN x").value() == [1, 2, 3]
    assert session.run("RETURN 1 AS x LIMIT 0").single() is None


# --- transactions -----------------------------------------------------------

def test_managed_transactions(driver):
    def create(tx, name):
        tx.run("CREATE (:Acc {name: $n, bal: 100})", n=name).consume()

    def total(tx):
        return tx.run("MATCH (a:Acc) RETURN sum(a.bal) AS s").single()["s"]

    with driver.session() as s:
        s.run("MATCH (n) DETACH DELETE n").consume()
        s.execute_write(create, "a")
        s.execute_write(create, "b")
        assert s.execute_read(total) == 200


def test_explicit_commit_and_rollback(session, driver):
    with session.begin_transaction() as tx:
        tx.run("CREATE (:T {v: 1})").consume()
        # Visible inside the transaction...
        assert tx.run("MATCH (t:T) RETURN count(t) AS c").single()["c"] == 1
        tx.commit()
    with session.begin_transaction() as tx:
        tx.run("CREATE (:T {v: 2})").consume()
        tx.rollback()
    with pytest.raises(RuntimeError):
        with session.begin_transaction() as tx:
            tx.run("CREATE (:T {v: 3})").consume()
            raise RuntimeError("leaving the block with an error rolls back")
    tx = session.begin_transaction()
    tx.run("CREATE (:T {v: 4})").consume()
    tx.close()  # closed without commit: rolled back
    with session.begin_transaction() as tx:
        tx.run("CREATE (:T {v: 5})").consume()
        # Leaving the block normally commits.
    assert session.run("MATCH (t:T) RETURN t.v AS v ORDER BY v").value() == [1, 5]


def test_several_open_results_in_one_transaction(driver):
    with driver.session(fetch_size=2) as s:
        with s.begin_transaction() as tx:
            r1 = tx.run("UNWIND range(1, 5) AS x RETURN x")
            r2 = tx.run("UNWIND range(10, 15) AS y RETURN y")
            assert [r["y"] for r in r2] == list(range(10, 16))
            assert [r["x"] for r in r1] == list(range(1, 6))
            tx.commit()


def test_transaction_metadata_and_timeout_config(session):
    q = Query("CREATE (:Meta) RETURN 1 AS x", metadata={"app": "smoke"}, timeout=10)
    assert session.run(q).single()["x"] == 1
    with session.begin_transaction(metadata={"k": "v"}, timeout=10) as tx:
        tx.run("RETURN 1").consume()
        tx.commit()


def test_query_timeout(session):
    q = Query("UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*)",
              timeout=0.2)
    with pytest.raises(ex.ClientError) as e:
        session.run(q).consume()
    assert "Transaction" in e.value.code
    # The session recovers.
    assert session.run("RETURN 1 AS x").single()["x"] == 1


def test_bookmarks_chain_sessions(driver):
    with driver.session() as s1:
        s1.run("CREATE (:Bm {v: 1})").consume()
        bookmarks = s1.last_bookmarks()
    with driver.session(bookmarks=bookmarks) as s2:
        assert s2.run("MATCH (b:Bm) RETURN count(b) AS c").single()["c"] >= 1
        s2.run("MATCH (b:Bm) DELETE b").consume()


def test_concurrent_sessions(driver):
    errors = []

    def work(i):
        try:
            with driver.session() as s:
                for j in range(20):
                    s.execute_write(lambda tx: tx.run(
                        "CREATE (:C {w: $i, j: $j})", i=i, j=j).consume())
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    driver.execute_query("MATCH (c:C) DELETE c")
    threads = [threading.Thread(target=work, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors
    records, _, _ = driver.execute_query("MATCH (c:C) RETURN count(c) AS n")
    assert records[0]["n"] == 160


# --- errors -----------------------------------------------------------------

def test_syntax_error(session):
    with pytest.raises(ex.CypherSyntaxError) as e:
        session.run("MATCH (n RETURN n").consume()
    assert e.value.code == "Neo.ClientError.Statement.SyntaxError"
    assert session.run("RETURN 1 AS x").single()["x"] == 1


def test_constraint_violation(session):
    session.run("CREATE CONSTRAINT smoke_u IF NOT EXISTS FOR (u:U) REQUIRE u.id IS UNIQUE").consume()
    try:
        session.run("CREATE (:U {id: 1})").consume()
        with pytest.raises(ex.ConstraintError):
            session.run("CREATE (:U {id: 1})").consume()
        with pytest.raises(ex.ConstraintError):
            session.execute_write(lambda tx: tx.run("CREATE (:U {id: 1})").consume())
        assert session.run("MATCH (u:U) RETURN count(u) AS c").single()["c"] == 1
    finally:
        session.run("DROP CONSTRAINT smoke_u").consume()


def test_error_inside_transaction_rolls_back(session):
    tx = session.begin_transaction()
    tx.run("CREATE (:E)").consume()
    with pytest.raises(ex.ClientError):
        tx.run("RETURN 1 / 0").consume()
    tx.close()
    assert session.run("MATCH (e:E) RETURN count(e) AS c").single()["c"] == 0


def test_unknown_database(driver):
    with driver.session(database="no_such_db") as s:
        with pytest.raises(ex.ClientError) as e:
            s.run("RETURN 1").consume()
    assert e.value.code == "Neo.ClientError.Database.DatabaseNotFound"


# --- databases --------------------------------------------------------------

def test_other_database(driver):
    # Administration goes through the `system` database, as with Neo4j.
    driver.execute_query("CREATE DATABASE smoke2 IF NOT EXISTS", database_="system")
    try:
        names = [r["name"] for r in driver.execute_query(
            "SHOW DATABASES", database_="system").records]
        assert "smoke2" in names
        driver.execute_query("CREATE (:OnlyHere)", database_="smoke2")
        r, _, _ = driver.execute_query("MATCH (n:OnlyHere) RETURN count(n) AS c",
                                       database_="smoke2")
        assert r[0]["c"] == 1
        r, _, _ = driver.execute_query("MATCH (n:OnlyHere) RETURN count(n) AS c")
        assert r[0]["c"] == 0
        with driver.session(database="smoke2") as s:
            assert s.run("MATCH (n) RETURN count(n) AS c").single()["c"] == 1
    finally:
        with driver.session(database="system") as s:
            s.run("DROP DATABASE smoke2 IF EXISTS").consume()


def test_system_database_rejects_queries(driver):
    with driver.session(database="system") as s:
        with pytest.raises(ex.ClientError):
            s.run("MATCH (n) RETURN n").consume()
