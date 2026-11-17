"""behave hooks for the TCK: one server for the whole run, and a record of every
scenario's outcome in results/results.tsv for run.py."""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path[:0] = [HERE, os.path.dirname(HERE)]

from graphbuff_server import Server  # noqa: E402

RESULTS = os.path.join(HERE, "results", "results.tsv")

# Live progress from run.py: "feature" (a line per feature file), "scenario" (a line per
# scenario) or "" (none). Written to the real stderr: behave captures stdout.
PROGRESS = os.environ.get("TCK_PROGRESS", "")
COLOR = sys.__stderr__.isatty() and not os.environ.get("NO_COLOR")
_MARK = {"passed": ("PASS", "32"), "failed": ("FAIL", "31"), "skipped": ("SKIP", "33")}


def _say(line):
    sys.__stderr__.write(line + "\n")
    sys.__stderr__.flush()


def _mark(status):
    text, color = _MARK.get(status, (status.upper()[:4], "0"))
    return f"\033[{color}m{text}\033[0m" if COLOR else text


def before_all(context):
    context.server = Server(["--test-procedures"]).start()
    context.tck_results = []


def before_feature(context, feature):
    context.tck_feature_start = len(context.tck_results)


def before_scenario(context, scenario):
    if "ignore" in scenario.effective_tags:
        scenario.skip("marked @ignore by the TCK")
    if not context.server.alive():  # an earlier query crashed it
        context.server.restart()
    context.params = {}
    context.result = None
    context.error = None
    context.before = None


def after_scenario(context, scenario):
    feature = os.path.relpath(scenario.feature.filename, os.path.join(HERE, "features"))
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
    if PROGRESS != "feature":
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
    if getattr(context, "server", None):
        context.server.stop()
    os.makedirs(os.path.dirname(RESULTS), exist_ok=True)
    with open(RESULTS, "w", encoding="utf-8", newline="\n") as f:
        for row in context.tck_results:
            f.write("\t".join(row) + "\n")
