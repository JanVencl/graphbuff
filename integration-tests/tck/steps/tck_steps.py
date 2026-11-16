"""Step definitions for the openCypher TCK, run against graphbuff-server over HTTP.

Every scenario gets a fresh in-memory database. Errors are checked by category only
(not by detail code or phase): graphbuff reports compile-time errors as SyntaxError or
SemanticError, and the TCK files all of those under SyntaxError, so either counts.
Side effects are computed as the TCK's reference driver does: by diffing the graph before
and after the query (node and relationship ids, the label names in use, and every
property as an (entity, key, value) triple).
"""

import json
import os

from behave import step, use_step_matcher

import tck_values
from graphbuff_server import QueryError

use_step_matcher("re")

DB = "tck"
QUERY_TIMEOUT_MS = 10_000
GRAPHS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "graphs")

# TCK error class -> graphbuff error codes that count as it.
ERRORS = {
    "SyntaxError": {"SyntaxError", "SemanticError"},
    "SemanticError": {"SyntaxError", "SemanticError"},
    "TypeError": {"TypeError"},
    "ArgumentError": {"RuntimeError", "TypeError"},
    "EntityNotFound": {"RuntimeError"},
    "ConstraintVerificationFailed": {"ConstraintError"},
    "ProcedureError": {"SemanticError", "RuntimeError"},
    "ParameterMissing": {"SemanticError", "RuntimeError"},
}


def _run(context, query):
    return context.server.query(query, context.params, db=DB, timeout_ms=QUERY_TIMEOUT_MS)


def _rows(table):
    """All rows of a behave table, the heading row included (TCK tables of parameters and
    side effects have no heading)."""
    return [list(table.headings)] + [list(r.cells) for r in table.rows]


def _fresh_graph(context):
    context.server.query(f"CREATE OR REPLACE DATABASE {DB}")


def _graph_state(context):
    nodes = context.server.query(
        "MATCH (n) RETURN id(n) AS id, labels(n) AS labels, properties(n) AS props", db=DB)["rows"]
    rels = context.server.query(
        "MATCH ()-[r]->() RETURN id(r) AS id, properties(r) AS props", db=DB)["rows"]
    state = {"nodes": set(), "relationships": set(), "labels": set(), "properties": set()}
    for i, labels, props in nodes:
        state["nodes"].add(i)
        state["labels"].update(labels)
        state["properties"].update(("n", i, k, json.dumps(v, sort_keys=True)) for k, v in props.items())
    for i, props in rels:
        state["relationships"].add(i)
        state["properties"].update(("r", i, k, json.dumps(v, sort_keys=True)) for k, v in props.items())
    return state


def _result(context):
    if context.error is not None:
        raise AssertionError(f"query failed: {context.error}")
    if context.result is None:
        raise AssertionError("no query was executed")
    return context.result


def _table_text(columns, rows):
    lines = ["| " + " | ".join(columns) + " |"]
    lines += ["| " + " | ".join(json.dumps(v) for v in row) + " |" for row in rows]
    return "\n".join(lines)


# ---- setup ------------------------------------------------------------------------------

@step(r"(?:an empty|any) graph")
def empty_graph(context):
    _fresh_graph(context)


@step(r"the (?P<name>[\w-]+) graph")
def named_graph(context, name):
    _fresh_graph(context)
    with open(os.path.join(GRAPHS, name, f"{name}.cypher"), encoding="utf-8") as f:
        _run(context, f.read())


@step(r"having executed:")
def having_executed(context):
    try:
        _run(context, context.text)
    except QueryError as e:
        raise AssertionError(f"setup query failed: {e}") from e


@step(r"parameters are:")
def parameters(context):
    for name, value in _rows(context.table):
        context.params[name] = tck_values.to_param(tck_values.parse(value))


@step(r"there exists a procedure (?P<signature>.+):")
def procedure(context, signature):
    raise NotImplementedError("test procedures cannot be registered over HTTP")


# ---- execution --------------------------------------------------------------------------

@step(r"executing query:")
def executing_query(context):
    context.before = _graph_state(context)
    _execute(context)


@step(r"executing control query:")
def executing_control_query(context):
    _execute(context)


def _execute(context):
    context.result, context.error = None, None
    try:
        context.result = _run(context, context.text)
    except QueryError as e:
        context.error = e


# ---- outcomes ---------------------------------------------------------------------------

@step(r"the result should be empty")
def result_empty(context):
    r = _result(context)
    assert not r["rows"], "expected no rows, got:\n" + _table_text(r["columns"], r["rows"])


@step(r"the result should be,? ?(?P<order>in any order|in order)?(?: ?\((?P<lists>ignoring element order for lists)\))?:")
def result_is(context, order, lists):
    r = _result(context)
    any_list_order = lists is not None
    header, *rows = _rows(context.table)
    assert header == r["columns"], f"expected columns {header}, got {r['columns']}"
    expected = [[tck_values.parse(c) for c in row] for row in rows]

    def row_eq(e, a):
        return len(e) == len(a) and all(tck_values.matches(x, y, any_list_order) for x, y in zip(e, a))

    if order == "in order":
        same = len(expected) == len(r["rows"]) and all(row_eq(e, a) for e, a in zip(expected, r["rows"]))
    else:
        same = tck_values.bag_match(expected, r["rows"], row_eq)
    if not same:
        want = "\n".join("| " + " | ".join(row) + " |" for row in rows)
        raise AssertionError(f"expected rows:\n{want}\ngot:\n{_table_text(r['columns'], r['rows'])}")


@step(r"no side effects#?")
def no_side_effects(context):
    _check_side_effects(context, {})


@step(r"the side effects should be:")
def side_effects(context):
    expected = {k: int(n) for k, n in _rows(context.table) if int(n) != 0}
    _check_side_effects(context, expected)


def _check_side_effects(context, expected):
    if context.before is None:
        raise AssertionError("no query was executed")
    after = _graph_state(context)
    actual = {}
    for kind in ("nodes", "relationships", "labels", "properties"):
        added = len(after[kind] - context.before[kind])
        removed = len(context.before[kind] - after[kind])
        if added:
            actual["+" + kind] = added
        if removed:
            actual["-" + kind] = removed
    assert actual == expected, f"side effects: expected {expected}, got {actual}"


@step(r"an? (?P<kind>\w+) should be raised at (?P<phase>[\w ]+): (?P<detail>.+)")
def error_raised(context, kind, phase, detail):
    if context.error is None:
        r = _result(context)
        raise AssertionError(f"expected {kind} ({detail}), got a result:\n"
                             + _table_text(r["columns"], r["rows"]))
    assert context.error.code in ERRORS.get(kind, ()), \
        f"expected {kind} ({detail}), got {context.error}"
