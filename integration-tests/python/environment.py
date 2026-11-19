"""behave hooks for both suites.

- tck/features: one server (with queries.test_procedures) for the whole run; each scenario gets a
  fresh database (see tck_steps). Every scenario's outcome goes to tck/results/results.tsv
  for run_tck.py.
- everything else (cucumber/features): every scenario starts on a fresh in-memory server
  on a free port. Steps that need other server options restart it with `restart_server`.
"""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

from graphbuff_server import Server  # noqa: E402

TESTS = os.path.dirname(HERE)
TCK_FEATURES = os.path.join(TESTS, "tck", "features")
TCK_RESULTS = os.path.join(TESTS, "tck", "results", "results.tsv")

# Live progress from run_tck.py: "feature" (a line per feature file), "scenario" (a line
# per scenario) or "" (none). Written to the real stderr: behave captures stdout.
PROGRESS = os.environ.get("TCK_PROGRESS", "")
COLOR = sys.__stderr__.isatty() and not os.environ.get("NO_COLOR")
_MARK = {"passed": ("PASS", "32"), "failed": ("FAIL", "31"), "skipped": ("SKIP", "33")}

_tck_server = None  # shared by every TCK scenario; started on the first one
_tck_ran = False  # a TCK feature ran: write the results file


def _say(line):
    sys.__stderr__.write(line + "\n")
    sys.__stderr__.flush()


def _mark(status):
    text, color = _MARK.get(status, (status.upper()[:4], "0"))
    return f"\033[{color}m{text}\033[0m" if COLOR else text


def _is_tck(feature):
    path = os.path.abspath(feature.filename)
    return os.path.commonpath([path, TCK_FEATURES]) == TCK_FEATURES


# ---- server per scenario (cucumber) ------------------------------------------------------

def restart_server(context, settings=None):
    """Stops the running server, if any, and starts one with `settings` (see Server)."""
    stop_server(context)
    context.server = Server(settings).start()
    context.base_url = context.server.url


def stop_server(context):
    server = getattr(context, "server", None)
    if server is not None:
        server.stop()
        context.server = None


# ---- hooks -------------------------------------------------------------------------------

def before_all(context):
    context.tck_results = []


def before_feature(context, feature):
    global _tck_ran
    context.tck = _is_tck(feature)
    context.tck_feature_start = len(context.tck_results)
    _tck_ran = _tck_ran or context.tck


def before_scenario(context, scenario):
    global _tck_server
    if not context.tck:
        restart_server(context)
        return
    if "ignore" in scenario.effective_tags:
        scenario.skip("marked @ignore by the TCK")
    if _tck_server is None:
        _tck_server = Server({"queries.test_procedures": True}).start()
    elif not _tck_server.alive():  # an earlier query crashed it
        _tck_server.restart()
    context.server = _tck_server
    context.params = {}
    context.result = None
    context.error = None
    context.before = None


def after_scenario(context, scenario):
    if not context.tck:
        stop_server(context)
        return
    feature = os.path.relpath(scenario.feature.filename, TCK_FEATURES)
    feature = os.path.splitext(feature)[0].replace(os.sep, "/")
    reason = ""
    status = scenario.status.name
    if status in ("failed", "error"):
        status = "failed"
        bad = next((s for s in scenario.steps if s.status.name in ("failed", "error", "undefined")), None)
        if bad is not None:
            exc = getattr(bad, "exception", None)
            if exc is not None and not isinstance(exc, AssertionError):
                why = f"{type(exc).__name__}: {exc}"
            else:
                why = getattr(bad, "error_message", None) or bad.status.name
            reason = f"line {bad.line}: {why}"
    reason = " ".join(reason.split())[:2000]
    context.tck_results.append((feature, scenario.name, status, reason))
    if PROGRESS == "scenario":
        _say(f"{_mark(status)} {feature} {scenario.name}" + (f"\n     {reason[:300]}" if reason else ""))


def after_feature(context, feature):
    if not context.tck or PROGRESS != "feature":
        return
    rows = context.tck_results[context.tck_feature_start:]
    if not rows:
        return
    passed = sum(r[2] == "passed" for r in rows)
    run = sum(r[2] != "skipped" for r in rows)
    total_passed = sum(r[2] == "passed" for r in context.tck_results)
    total_run = sum(r[2] != "skipped" for r in context.tck_results)
    status = "passed" if passed == run else "failed"
    _say(f"{_mark(status)} {rows[0][0]:<52} {passed:>4}/{run:<4}"
         f"  total {total_passed}/{total_run} ({100.0 * total_passed / max(total_run, 1):.1f} %)")


def after_all(context):
    global _tck_server
    if _tck_server is not None:
        _tck_server.stop()
        _tck_server = None
    if not _tck_ran:
        return
    os.makedirs(os.path.dirname(TCK_RESULTS), exist_ok=True)
    with open(TCK_RESULTS, "w", encoding="utf-8", newline="\n") as f:
        for row in context.tck_results:
            f.write("\t".join(row) + "\n")
