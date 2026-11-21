"""Starts and stops a graphbuff-server for the Python test suites, and runs queries on it.

The binary is the one named by GRAPHBUFF_SERVER, else target/release or target/debug
(whichever was built last).
"""

import collections
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time

import requests

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))


def server_binary():
    explicit = os.environ.get("GRAPHBUFF_SERVER")
    if explicit:
        return explicit
    exe = "graphbuff-server.exe" if sys.platform == "win32" else "graphbuff-server"
    built = [os.path.join(ROOT, "target", p, exe) for p in ("release", "debug")]
    built = [p for p in built if os.path.exists(p)]
    if not built:
        raise RuntimeError("graphbuff-server is not built: run `cargo build -p graphbuff-server` "
                           "or set GRAPHBUFF_SERVER")
    return max(built, key=os.path.getmtime)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# The token test servers accept, unless a test gives its own.
TOKEN = "integration-test-token"


def toml_value(v):
    """A Python value as TOML: strings as literal strings ('…', so Windows paths need no
    escapes) unless they hold a quote."""
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, int):
        return str(v)
    v = str(v)
    return f"'{v}'" if "'" not in v and "\n" not in v else json.dumps(v)


def write_config(settings, directory=None):
    """Writes `settings` ({"section.key": value}) as a graphbuff.toml in `directory` (a new
    temporary folder by default); returns the file's path."""
    sections = {}
    for name, value in settings.items():
        section, key = name.split(".", 1)
        sections.setdefault(section, []).append(f"{key} = {toml_value(value)}")
    text = "\n".join(f"[{s}]\n" + "\n".join(lines) + "\n" for s, lines in sections.items())
    directory = directory or tempfile.mkdtemp(prefix="graphbuff-config-")
    path = os.path.join(directory, "graphbuff.toml")
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    return path


def clean_env(**extra):
    """The environment without the caller's GRAPHBUFF_* variables, plus `extra`."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPHBUFF_")}
    env.update(extra)
    return env


class QueryError(Exception):
    """An error answer: `code` is the error kind (SyntaxError, TypeError, ...)."""

    def __init__(self, code, message):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class ServerDied(Exception):
    """The server process exited while answering a request."""


class Server:
    """One server process on a free port. `settings` ({"section.key": value}) go into its
    config file; without `storage.data`, databases live in memory. Clients log in with the
    token: `security.token` from `settings`, else `TOKEN`; `http` sends it with every
    request."""

    def __init__(self, settings=None):
        self.settings = dict(settings or {})
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.token = self.settings.get("security.token", TOKEN)
        self.http = self._session()
        self.proc = None

    def _session(self):
        http = requests.Session()
        http.headers["Authorization"] = f"Bearer {self.token}"
        return http

    def start(self):
        settings = {"server.bind": f"127.0.0.1:{self.port}", "logs.level": "error", "logs.dir": "-",
                    "security.token": self.token}
        if "storage.data" not in self.settings:
            settings["storage.mode"] = "memory"
        settings.update(self.settings)
        cmd = [server_binary(), "--config", write_config(settings)]
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, env=clean_env())
        # Read stderr all along: the server writes an audit line per admin command, and a
        # full pipe (4 KB on Windows) would block it mid-command, holding its locks.
        self.stderr_tail = collections.deque(maxlen=100)
        self._stderr_reader = threading.Thread(target=self._drain_stderr, args=(self.proc, self.stderr_tail),
                                               daemon=True)
        self._stderr_reader.start()
        deadline = time.time() + 20
        while True:
            try:
                if self.http.get(self.url + "/health", timeout=1).status_code == 200:
                    return self
            except requests.ConnectionError:
                pass
            if self.proc.poll() is not None or time.time() > deadline:
                self.proc.kill()
                raise RuntimeError("graphbuff-server did not start: " + self._stderr_text())
            time.sleep(0.05)

    @staticmethod
    def _drain_stderr(proc, tail):
        for line in proc.stderr:
            tail.append(line.decode(errors="replace"))

    def _stderr_text(self):
        """The end of the server's standard error, once it has exited."""
        self._stderr_reader.join(5)
        return "".join(self.stderr_tail).strip()

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def exit_message(self):
        """How the process ended, with the end of its standard error."""
        if self.proc is None or self.proc.poll() is None:
            return "server is running"
        err = self._stderr_text()
        return f"server exited with code {self.proc.returncode}: {err[-500:]}"

    def restart(self):
        self.stop()
        self.http = self._session()
        return self.start()

    def stop(self):
        if self.proc is not None:
            self.proc.terminate()
            self.proc.wait(10)
            self.proc = None
        self.http.close()

    def query(self, query, params=None, db=None, timeout_ms=None):
        """Runs `query`; returns the JSON answer ({"columns", "rows", "stats"}) or raises
        QueryError."""
        body = {"query": query}
        if params:
            body["params"] = params
        if timeout_ms:
            body["timeoutMs"] = timeout_ms
        path = f"/db/{db}/query" if db else "/query"
        # Allow the server's own limit plus a margin before giving up on the request.
        wait = (timeout_ms or 60_000) / 1000 + 30
        try:
            answer = self.http.post(self.url + path, json=body, timeout=wait).json()
        except requests.RequestException as e:
            try:  # a crashing process may still be exiting
                self.proc.wait(2)
            except subprocess.TimeoutExpired:
                pass
            if not self.alive():
                raise ServerDied(self.exit_message()) from e
            raise
        if "error" in answer:
            raise QueryError(answer["error"].get("code", "?"), answer["error"].get("message", ""))
        return answer
