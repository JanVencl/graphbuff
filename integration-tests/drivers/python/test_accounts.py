"""User accounts, roles and TLS with the official Neo4j Python driver.

Starts its own servers: one with `--users` (accounts made with the offline
`graphbuff-server users` command) and, when the binary was built with TLS (`--features
tls` or `tls-rustcrypto`), one serving Bolt over TLS with the test certificate in
`crates/server/tests/tls/`.
"""

import os
import subprocess
import tempfile
import time
import urllib.request

import pytest
from neo4j import GraphDatabase, TrustCustomCAs
from neo4j import exceptions as ex

from graphbuff_server import ROOT, free_port as _free_port, server_binary as _server_binary

CERTS = os.path.join(ROOT, "crates", "server", "tests", "tls")


def _start(args, health_url, context=None):
    proc = subprocess.Popen([_server_binary(), *args], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = time.time() + 20
    while True:
        try:
            urllib.request.urlopen(health_url, timeout=1, context=context)
            return proc
        except OSError:
            if proc.poll() is not None or time.time() > deadline:
                proc.kill()
                raise RuntimeError("server did not start: " + proc.stderr.read().decode())
            time.sleep(0.1)


def _add_user(users_file, name, password, *roles):
    env = dict(os.environ, GRAPHBUFF_PASSWORD=password)
    subprocess.run([_server_binary(), "users", users_file, "add", name, *roles],
                   check=True, env=env, stdout=subprocess.DEVNULL)


@pytest.fixture(scope="module")
def accounts():
    with tempfile.TemporaryDirectory() as tmp:
        users_file = os.path.join(tmp, "users")
        _add_user(users_file, "root", "rootpw", "admin")
        _add_user(users_file, "rita", "ritapw", "reader")
        http_port, bolt_port = _free_port(), _free_port()
        proc = _start(["--bind", f"127.0.0.1:{http_port}", "--bolt", f"127.0.0.1:{bolt_port}",
                       "--users", users_file, "--in-memory"],
                      f"http://127.0.0.1:{http_port}/health")
        yield f"bolt://127.0.0.1:{bolt_port}"
        proc.terminate()
        proc.wait(10)


def test_users_log_in_with_their_passwords(accounts):
    with GraphDatabase.driver(accounts, auth=("root", "rootpw")) as d:
        d.verify_connectivity()
    with GraphDatabase.driver(accounts, auth=("root", "wrong")) as d:
        with pytest.raises(ex.AuthError):
            d.verify_connectivity()
    with GraphDatabase.driver(accounts, auth=("ghost", "rootpw")) as d:
        with pytest.raises(ex.AuthError):
            d.verify_connectivity()


def test_readers_cannot_write(accounts):
    with GraphDatabase.driver(accounts, auth=("rita", "ritapw")) as d:
        records, _, _ = d.execute_query("RETURN 1 AS x")
        assert records[0]["x"] == 1
        with pytest.raises(ex.Forbidden):
            d.execute_query("CREATE (n)")
        with pytest.raises(ex.Forbidden):
            d.execute_query("SHOW USERS", database_="system")
        records, _, _ = d.execute_query("SHOW CURRENT USER", database_="system")
        assert records[0]["user"] == "rita"
        assert records[0]["roles"] == ["reader"]


def test_admins_manage_users(accounts):
    with GraphDatabase.driver(accounts, auth=("root", "rootpw")) as admin:
        admin.execute_query("CREATE USER eve IF NOT EXISTS SET PASSWORD $pw CHANGE NOT REQUIRED",
                            pw="evepw", database_="system")
        admin.execute_query("GRANT ROLE editor TO eve", database_="system")
        records, summary, _ = admin.execute_query("SHOW USERS", database_="system")
        assert {r["user"] for r in records} >= {"root", "rita", "eve"}

        with GraphDatabase.driver(accounts, auth=("eve", "evepw")) as eve:
            _, summary, _ = eve.execute_query("CREATE (:Note {by: 'eve'})")
            assert summary.counters.nodes_created == 1
            with eve.session(database="system") as s:
                s.run("ALTER CURRENT USER SET PASSWORD FROM $old TO $new", old="evepw", new="evepw2").consume()
        with GraphDatabase.driver(accounts, auth=("eve", "evepw2")) as eve:
            eve.verify_connectivity()

        admin.execute_query("DROP USER eve", database_="system")
        with GraphDatabase.driver(accounts, auth=("eve", "evepw2")) as eve:
            with pytest.raises(ex.AuthError):
                eve.verify_connectivity()


# --- TLS --------------------------------------------------------------------

@pytest.fixture(scope="module")
def tls_server():
    import ssl
    http_port, bolt_port = _free_port(), _free_port()
    args = ["--bind", f"127.0.0.1:{http_port}", "--bolt", f"127.0.0.1:{bolt_port}", "--in-memory",
            "--tls-cert", os.path.join(CERTS, "cert.pem"), "--tls-key", os.path.join(CERTS, "key.pem")]
    context = ssl.create_default_context(cafile=os.path.join(CERTS, "ca.pem"))
    try:
        proc = _start(args, f"https://localhost:{http_port}/health", context)
    except RuntimeError as e:
        if "feature" in str(e):
            pytest.skip("graphbuff-server was built without TLS (cargo build --features tls-rustcrypto)")
        raise
    yield bolt_port
    proc.terminate()
    proc.wait(10)


def test_bolt_over_tls_with_a_trusted_ca(tls_server):
    uri = f"bolt://localhost:{tls_server}"
    with GraphDatabase.driver(uri, auth=("neo4j", "x"), encrypted=True,
                              trusted_certificates=TrustCustomCAs(os.path.join(CERTS, "ca.pem"))) as d:
        records, _, _ = d.execute_query("UNWIND range(1, 3000) AS i RETURN sum(i) AS s")
        assert records[0]["s"] == 4501500


def test_bolt_ssc_scheme(tls_server):
    with GraphDatabase.driver(f"bolt+ssc://127.0.0.1:{tls_server}", auth=("neo4j", "x")) as d:
        d.verify_connectivity()
    with GraphDatabase.driver(f"neo4j+ssc://127.0.0.1:{tls_server}", auth=("neo4j", "x")) as d:
        records, _, _ = d.execute_query("RETURN 'secure' AS x")
        assert records[0]["x"] == "secure"


def test_untrusted_certificates_are_refused(tls_server):
    with GraphDatabase.driver(f"bolt+s://localhost:{tls_server}", auth=("neo4j", "x")) as d:
        with pytest.raises(ex.ServiceUnavailable):
            d.verify_connectivity()


def test_plain_bolt_is_refused(tls_server):
    with GraphDatabase.driver(f"bolt://localhost:{tls_server}", auth=("neo4j", "x")) as d:
        with pytest.raises(ex.ServiceUnavailable):
            d.verify_connectivity()
