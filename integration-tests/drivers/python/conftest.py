"""Starts one graphbuff-server (HTTP + Bolt, token auth) for the whole session."""

import os
import socket
import subprocess
import sys
import time
import urllib.request

import pytest
from neo4j import GraphDatabase

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
TOKEN = "smoke-secret"


def _server_binary():
    explicit = os.environ.get("GRAPHBUFF_SERVER")
    if explicit:
        return explicit
    exe = "graphbuff-server.exe" if sys.platform == "win32" else "graphbuff-server"
    for profile in ("debug", "release"):
        path = os.path.join(ROOT, "target", profile, exe)
        if os.path.exists(path):
            return path
    pytest.exit("graphbuff-server not built: run `cargo build -p graphbuff-server` "
                "or set GRAPHBUFF_SERVER", returncode=2)


def _free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="session")
def server():
    http_port, bolt_port = _free_port(), _free_port()
    cmd = [_server_binary(),
           "--bind", f"127.0.0.1:{http_port}",
           "--bolt", f"127.0.0.1:{bolt_port}",
           "--token", TOKEN,
           "--tx-timeout", "5", "--in-memory"]
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.time() + 20
    while True:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{http_port}/health", timeout=1)
            break
        except OSError:
            if proc.poll() is not None or time.time() > deadline:
                proc.kill()
                raise RuntimeError("server did not start: " + proc.stderr.read().decode())
            time.sleep(0.1)
    yield {"bolt": f"bolt://127.0.0.1:{bolt_port}",
           "neo4j": f"neo4j://127.0.0.1:{bolt_port}",
           "auth": ("neo4j", TOKEN)}
    proc.terminate()
    proc.wait(10)


@pytest.fixture(scope="session")
def driver(server):
    with GraphDatabase.driver(server["bolt"], auth=server["auth"]) as d:
        yield d


@pytest.fixture
def session(driver):
    driver.execute_query("MATCH (n) DETACH DELETE n")
    with driver.session() as s:
        yield s
