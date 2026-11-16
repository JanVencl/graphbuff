"""Starts and stops a graphbuff-server for the Python test suites, and runs queries on it.

The binary is the one named by GRAPHBUFF_SERVER, else target/release or target/debug
(whichever was built last).
"""

import os
import socket
import subprocess
import sys
import time

import requests

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))


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


class QueryError(Exception):
    """An error answer: `code` is the error kind (SyntaxError, TypeError, ...)."""

    def __init__(self, code, message):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class ServerDied(Exception):
    """The server process exited while answering a request."""


class Server:
    """One server process on a free port. `args` are extra command-line options; without
    `--data`, databases live in memory."""

    def __init__(self, args=()):
        self.args = list(args)
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.http = requests.Session()
        self.proc = None

    def start(self):
        cmd = [server_binary(), "--bind", f"127.0.0.1:{self.port}", "--log-level", "error"]
        if "--data" not in self.args:
            cmd.append("--in-memory")
        cmd += self.args
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.time() + 20
        while True:
            try:
                if self.http.get(self.url + "/health", timeout=1).status_code == 200:
                    return self
            except requests.ConnectionError:
                pass
            if self.proc.poll() is not None or time.time() > deadline:
                self.proc.kill()
                raise RuntimeError("graphbuff-server did not start: " + self.proc.stderr.read().decode())
            time.sleep(0.05)

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def exit_message(self):
        """How the process ended, with the end of its standard error."""
        if self.proc is None or self.proc.poll() is None:
            return "server is running"
        err = self.proc.stderr.read().decode(errors="replace").strip()
        return f"server exited with code {self.proc.returncode}: {err[-500:]}"

    def restart(self):
        self.stop()
        self.http = requests.Session()
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
