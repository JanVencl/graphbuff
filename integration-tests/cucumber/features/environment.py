"""behave hooks: every scenario starts on a fresh in-memory server (on a free port).
Steps that need other server options restart it with `restart_server`."""

import os
import sys

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..")))

from graphbuff_server import Server  # noqa: E402


def restart_server(context, args=()):
    """Stops the running server, if any, and starts one with `args`."""
    stop_server(context)
    context.server = Server(args).start()
    context.base_url = context.server.url


def stop_server(context):
    server = getattr(context, "server", None)
    if server is not None:
        server.stop()
        context.server = None


def before_scenario(context, scenario):
    restart_server(context)


def after_scenario(context, scenario):
    stop_server(context)
