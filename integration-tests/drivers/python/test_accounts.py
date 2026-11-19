"""User accounts, roles and TLS with the official Neo4j Python driver.

Starts its own servers: one with user accounts in the system database of a data
directory (made with the offline `graphbuff-server admin init` and `users` commands) and,
when the binary was built with TLS (`--features tls` or `tls-rustcrypto`), one serving
Bolt over TLS with the test certificate in `crates/server/tests/tls/`.
"""

import os
import subprocess
import tempfile
import time
import urllib.request

import pytest
from neo4j import GraphDatabase, TrustCustomCAs
from neo4j import exceptions as ex

from graphbuff_server import ROOT, clean_env as _clean_env, free_port as _free_port, \
    server_binary as _server_binary, write_config

CERTS = os.path.join(ROOT, "crates", "server", "tests", "tls")


def _start(settings, health_url, context=None, env=None):
    """A server with the config `settings` ({"section.key": value})."""
    settings = {"logs.dir": "-", **settings}
    proc = subprocess.Popen([_server_binary(), "--config", write_config(settings)], stdout=subprocess.DEVNULL,
                            stderr=subprocess.PIPE, env=env if env is not None else _clean_env())
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


def _offline(data, *args, stdin=None, **env):
    """`graphbuff-server ARGS…` on the data directory `data`, while no server uses it."""
    command, rest = args[0], args[1:]
    config = write_config({"storage.data": os.path.abspath(data)})
    return subprocess.run([_server_binary(), command, "--config", config, *rest], input=stdin, text=True,
                          capture_output=True, env=_clean_env(**env))


def _add_user(data, name, password, *roles):
    _offline(data, "users", "add", name, *roles, GRAPHBUFF_PASSWORD=password).check_returncode()


def _serve(data, **settings):
    """A server on `data` with Bolt and more `settings`; returns the process and the Bolt URL."""
    http_port, bolt_port = _free_port(), _free_port()
    proc = _start({"server.bind": f"127.0.0.1:{http_port}", "server.bolt": f"127.0.0.1:{bolt_port}",
                   "storage.data": os.path.abspath(data), **settings},
                  f"http://127.0.0.1:{http_port}/health")
    return proc, f"bolt://127.0.0.1:{bolt_port}"


def _stop(proc):
    proc.terminate()
    proc.wait(10)


@pytest.fixture(scope="module")
def accounts():
    with tempfile.TemporaryDirectory() as tmp:
        data = os.path.join(tmp, "data")
        _offline(data, "admin", "init", "root", GRAPHBUFF_ADMIN_PASSWORD="root-password").check_returncode()
        _add_user(data, "rita", "rita-password", "reader")
        proc, url = _serve(data)
        yield url
        _stop(proc)


def test_users_log_in_with_their_passwords(accounts):
    with GraphDatabase.driver(accounts, auth=("root", "root-password")) as d:
        d.verify_connectivity()
    with GraphDatabase.driver(accounts, auth=("root", "wrong")) as d:
        with pytest.raises(ex.AuthError):
            d.verify_connectivity()
    with GraphDatabase.driver(accounts, auth=("ghost", "root-password")) as d:
        with pytest.raises(ex.AuthError):
            d.verify_connectivity()


def test_readers_cannot_write(accounts):
    with GraphDatabase.driver(accounts, auth=("rita", "rita-password")) as d:
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
    with GraphDatabase.driver(accounts, auth=("root", "root-password")) as admin:
        admin.execute_query("CREATE USER eve IF NOT EXISTS SET PASSWORD $pw CHANGE NOT REQUIRED",
                            pw="eve-password", database_="system")
        admin.execute_query("GRANT ROLE editor TO eve", database_="system")
        records, summary, _ = admin.execute_query("SHOW USERS", database_="system")
        assert {r["user"] for r in records} >= {"root", "rita", "eve"}

        with GraphDatabase.driver(accounts, auth=("eve", "eve-password")) as eve:
            _, summary, _ = eve.execute_query("CREATE (:Note {by: 'eve'})")
            assert summary.counters.nodes_created == 1
            with eve.session(database="system") as s:
                s.run("ALTER CURRENT USER SET PASSWORD FROM $old TO $new", old="eve-password", new="eve-password-2").consume()
        with GraphDatabase.driver(accounts, auth=("eve", "eve-password-2")) as eve:
            eve.verify_connectivity()

        admin.execute_query("DROP USER eve", database_="system")
        with GraphDatabase.driver(accounts, auth=("eve", "eve-password-2")) as eve:
            with pytest.raises(ex.AuthError):
                eve.verify_connectivity()


def test_the_last_administrator_cannot_go():
    """Not even the token holder (an administrator, but no account) may leave none."""
    with tempfile.TemporaryDirectory() as tmp:
        data = os.path.join(tmp, "data")
        _offline(data, "admin", "init", "root", GRAPHBUFF_ADMIN_PASSWORD="root-password").check_returncode()
        proc, url = _serve(data, **{"security.token": "tok"})
        try:
            with GraphDatabase.driver(url, auth=("neo4j", "tok")) as token:
                for statement in ("DROP USER root", "REVOKE ROLE admin FROM root"):
                    with pytest.raises(ex.ClientError, match="no administrator"):
                        token.execute_query(statement, database_="system")
            with GraphDatabase.driver(url, auth=("root", "root-password")) as root:
                records, _, _ = root.execute_query("SHOW CURRENT USER", database_="system")
                assert records[0]["roles"] == ["admin"]
        finally:
            _stop(proc)


def test_offline_admin_init_and_reset_password():
    """`admin init` once, `admin reset-password` to get back in; neither while a server runs."""
    with tempfile.TemporaryDirectory() as tmp:
        data = os.path.join(tmp, "data")
        init = _offline(data, "admin", "init", stdin="first-password\n")
        assert init.returncode == 0, init.stderr
        again = _offline(data, "admin", "init", stdin="other-password\n")
        assert again.returncode != 0 and "administrator already (admin)" in again.stderr

        proc, url = _serve(data)
        try:
            busy = _offline(data, "admin", "reset-password", "admin", GRAPHBUFF_PASSWORD="second-password")
            assert busy.returncode != 0 and "is a server using it" in busy.stderr
            with GraphDatabase.driver(url, auth=("admin", "first-password")) as d:
                d.verify_connectivity()
        finally:
            _stop(proc)

        reset = _offline(data, "admin", "reset-password", "admin", GRAPHBUFF_PASSWORD="second-password")
        assert reset.returncode == 0, reset.stderr
        proc, url = _serve(data)
        try:
            with GraphDatabase.driver(url, auth=("admin", "second-password")) as d:
                records, _, _ = d.execute_query("SHOW CURRENT USER", database_="system")
                assert records[0]["roles"] == ["admin"]
            with GraphDatabase.driver(url, auth=("admin", "first-password")) as d:
                with pytest.raises(ex.AuthError):
                    d.verify_connectivity()
        finally:
            _stop(proc)


def test_first_admin_from_the_environment():
    """GRAPHBUFF_ADMIN_* create the administrator when there are no accounts (as in Docker)."""
    with tempfile.TemporaryDirectory() as tmp:
        secret = os.path.join(tmp, "secret")
        with open(secret, "w") as f:
            f.write("boot-password\n")
        env = _clean_env(GRAPHBUFF_ADMIN_USER="boot", GRAPHBUFF_ADMIN_PASSWORD_FILE=secret)
        http_port, bolt_port = _free_port(), _free_port()
        proc = _start({"server.bind": f"127.0.0.1:{http_port}", "server.bolt": f"127.0.0.1:{bolt_port}",
                       "storage.mode": "memory"},
                      f"http://127.0.0.1:{http_port}/health", env=env)
        try:
            url = f"bolt://127.0.0.1:{bolt_port}"
            with GraphDatabase.driver(url, auth=("boot", "boot-password")) as d:
                records, _, _ = d.execute_query("SHOW CURRENT USER", database_="system")
                assert records[0]["roles"] == ["admin"]
            with GraphDatabase.driver(url, auth=("boot", "boot-password\n")) as d:
                with pytest.raises(ex.AuthError):
                    d.verify_connectivity()
        finally:
            proc.terminate()
            proc.wait(10)


# --- TLS --------------------------------------------------------------------

@pytest.fixture(scope="module")
def tls_server():
    import ssl
    http_port, bolt_port = _free_port(), _free_port()
    args = {"server.bind": f"127.0.0.1:{http_port}", "server.bolt": f"127.0.0.1:{bolt_port}",
            "storage.mode": "memory", "security.token": "x",
            "security.tls_cert": os.path.join(CERTS, "cert.pem"), "security.tls_key": os.path.join(CERTS, "key.pem")}
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
