"""Starts one graphbuff-server (HTTP + Bolt, token auth) for the whole session."""

import os
import sys

import pytest
from neo4j import GraphDatabase

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", ".."))
from graphbuff_server import Server, free_port  # noqa: E402

TOKEN = "smoke-secret"


@pytest.fixture(scope="session")
def server():
    bolt_port = free_port()
    try:
        srv = Server(["--bolt", f"127.0.0.1:{bolt_port}", "--token", TOKEN, "--tx-timeout", "5"]).start()
    except RuntimeError as e:
        pytest.exit(str(e), returncode=2)
    yield {"bolt": f"bolt://127.0.0.1:{bolt_port}",
           "neo4j": f"neo4j://127.0.0.1:{bolt_port}",
           "auth": ("neo4j", TOKEN)}
    srv.stop()


@pytest.fixture(scope="session")
def driver(server):
    with GraphDatabase.driver(server["bolt"], auth=server["auth"]) as d:
        yield d


@pytest.fixture
def session(driver):
    driver.execute_query("MATCH (n) DETACH DELETE n")
    with driver.session() as s:
        yield s
