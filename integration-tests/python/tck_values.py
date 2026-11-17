"""Values in the TCK's result notation, and how they compare with graphbuff's JSON.

The notation is Cypher literals plus graph entities: nodes `(:A:B {k: 1})`,
relationships `[:T {k: 1}]` and paths `<(:A)-[:T]->(:B)<-[:U]-()>`. Temporal values are
written as strings (`'2015-07-21'`), the form graphbuff's JSON uses too.

Parsed values: None, bool, int, float, str, list, dict (a map), and Node, Rel, Path.
"""

import math
from dataclasses import dataclass, field


@dataclass
class Node:
    labels: list
    props: dict = field(default_factory=dict)


@dataclass
class Rel:
    type: str
    props: dict = field(default_factory=dict)


@dataclass
class Path:
    start: Node
    # (relationship, points forward, next node)
    steps: list


class ParseError(ValueError):
    pass


def parse(text):
    """Parses one value; the whole text must be consumed."""
    p = _Parser(text)
    v = p.value()
    p.ws()
    if p.i != len(text):
        p.fail("trailing input")
    return v


class _Parser:
    def __init__(self, text):
        self.s = text
        self.i = 0

    def fail(self, msg):
        raise ParseError(f"{msg} at {self.i} in `{self.s}`")

    def ws(self):
        while self.i < len(self.s) and self.s[self.i].isspace():
            self.i += 1

    def peek(self):
        self.ws()
        return self.s[self.i] if self.i < len(self.s) else ""

    def eat(self, token):
        self.ws()
        if self.s.startswith(token, self.i):
            self.i += len(token)
            return True
        return False

    def expect(self, token):
        if not self.eat(token):
            self.fail(f"expected `{token}`")

    def word(self, w):
        self.ws()
        end = self.i + len(w)
        if self.s.startswith(w, self.i) and not (end < len(self.s) and (self.s[end].isalnum() or self.s[end] == "_")):
            self.i = end
            return True
        return False

    def value(self):
        c = self.peek()
        if not c:
            self.fail("expected a value")
        if c in "'\"":
            return self.string()
        if c == "[":
            save = self.i
            self.i += 1
            if self.peek() == ":":
                self.i = save
                return self.rel()
            items = []
            if not self.eat("]"):
                while True:
                    items.append(self.value())
                    if self.eat("]"):
                        break
                    self.expect(",")
            return items
        if c == "{":
            return self.map()
        if c == "(":
            return self.node()
        if c == "<":
            return self.path()
        for w, v in (("null", None), ("true", True), ("false", False),
                     ("NaN", math.nan), ("Infinity", math.inf)):
            if self.word(w):
                return v
        if self.eat("-Infinity"):
            return -math.inf
        return self.number()

    def number(self):
        self.ws()
        start = self.i
        if self.i < len(self.s) and self.s[self.i] in "+-":
            self.i += 1
        is_float = False
        while self.i < len(self.s):
            c = self.s[self.i]
            if c.isdigit():
                pass
            elif c in ".eE":
                is_float = True
            elif c in "+-" and self.s[self.i - 1] in "eE":
                pass
            else:
                break
            self.i += 1
        t = self.s[start:self.i]
        if t in ("", "+", "-"):
            self.fail("expected a value")
        try:
            return float(t) if is_float else int(t)
        except ValueError:
            self.fail(f"bad number `{t}`")

    def string(self):
        quote = self.s[self.i]
        self.i += 1
        out = []
        escapes = {"n": "\n", "t": "\t", "r": "\r", "b": "\b", "f": "\f"}
        while True:
            if self.i >= len(self.s):
                self.fail("unterminated string")
            c = self.s[self.i]
            self.i += 1
            if c == quote:
                return "".join(out)
            if c != "\\":
                out.append(c)
                continue
            if self.i >= len(self.s):
                self.fail("unterminated string")
            e = self.s[self.i]
            self.i += 1
            if e == "u":
                out.append(chr(int(self.s[self.i:self.i + 4], 16)))
                self.i += 4
            else:
                out.append(escapes.get(e, e))

    def name(self):
        self.ws()
        if self.s.startswith("`", self.i):
            end = self.s.find("`", self.i + 1)
            if end < 0:
                self.fail("unterminated `name`")
            n = self.s[self.i + 1:end]
            self.i = end + 1
            return n
        start = self.i
        while self.i < len(self.s) and (self.s[self.i].isalnum() or self.s[self.i] == "_"):
            self.i += 1
        if self.i == start:
            self.fail("expected a name")
        return self.s[start:self.i]

    def map(self):
        self.expect("{")
        m = {}
        if self.eat("}"):
            return m
        while True:
            k = self.name()
            self.expect(":")
            m[k] = self.value()
            if self.eat("}"):
                return m
            self.expect(",")

    def props(self):
        return self.map() if self.peek() == "{" else {}

    def node(self):
        self.expect("(")
        labels = []
        while self.eat(":"):
            labels.append(self.name())
        props = self.props()
        self.expect(")")
        return Node(labels, props)

    def rel(self):
        self.expect("[")
        self.expect(":")
        t = self.name()
        props = self.props()
        self.expect("]")
        return Rel(t, props)

    def path(self):
        self.expect("<")
        start = self.node()
        steps = []
        while not self.eat(">"):
            backward = self.eat("<-")
            if not backward:
                self.expect("-")
            rel = self.rel()
            forward = self.eat("->")
            if not forward:
                self.expect("-")
            if forward == backward:
                self.fail("a path relationship needs exactly one direction")
            steps.append((rel, forward, self.node()))
        return Path(start, steps)


def to_param(v):
    """A parsed value as a JSON query parameter."""
    if isinstance(v, (Node, Rel, Path)):
        raise ValueError("a parameter cannot be a graph entity")
    if isinstance(v, list):
        return [to_param(x) for x in v]
    if isinstance(v, dict):
        return {k: to_param(x) for k, x in v.items()}
    return v


# ---- comparison -------------------------------------------------------------------------

_SPECIAL_FLOATS = {"NaN", "Infinity", "-Infinity"}


def _float_eq(e, a):
    if isinstance(a, str) and a in _SPECIAL_FLOATS:  # JSON has no NaN or infinities
        a = float(a.replace("Infinity", "inf"))
    if not isinstance(a, float):
        return False
    return e == a or (math.isnan(e) and math.isnan(a))


def _is_int(a):
    return isinstance(a, int) and not isinstance(a, bool)


def _entity(a, kind):
    return isinstance(a, dict) and a.get("$type") == kind


def matches(e, a, any_list_order=False):
    """Whether the JSON value `a` is the value `e` describes. With `any_list_order`,
    lists compare as bags."""
    if e is None:
        return a is None
    if isinstance(e, bool):
        return isinstance(a, bool) and a == e
    if isinstance(e, int):
        return _is_int(a) and a == e
    if isinstance(e, float):
        return _float_eq(e, a)
    if isinstance(e, str):
        return isinstance(a, str) and a == e
    if isinstance(e, list):
        if not isinstance(a, list) or len(a) != len(e):
            return False
        if any_list_order:
            return bag_match(e, a, lambda x, y: matches(x, y, True))
        return all(matches(x, y, False) for x, y in zip(e, a))
    if isinstance(e, dict):
        return (isinstance(a, dict) and "$type" not in a and a.keys() == e.keys()
                and all(matches(v, a[k], any_list_order) for k, v in e.items()))
    if isinstance(e, Node):
        return _entity(a, "node") and _node_matches(e, a)
    if isinstance(e, Rel):
        return _entity(a, "relationship") and _rel_matches(e, a)
    if isinstance(e, Path):
        return _entity(a, "path") and _path_matches(e, a)
    raise TypeError(f"unexpected parsed value {e!r}")


def _props_match(e, a):
    return a.keys() == e.keys() and all(matches(v, a[k]) for k, v in e.items())


def _node_matches(e, a):
    return set(e.labels) == set(a["labels"]) and _props_match(e.props, a["properties"])


def _rel_matches(e, a):
    return e.type == a["type"] and _props_match(e.props, a["properties"])


def _path_matches(e, a):
    nodes, rels = a["nodes"], a["relationships"]
    if len(nodes) != len(e.steps) + 1 or len(rels) != len(e.steps):
        return False
    if not _node_matches(e.start, nodes[0]):
        return False
    for i, (rel, forward, node) in enumerate(e.steps):
        r, frm, to = rels[i], nodes[i]["id"], nodes[i + 1]["id"]
        ends = (r["start"], r["end"]) == ((frm, to) if forward else (to, frm))
        if not (ends and _rel_matches(rel, r) and _node_matches(node, nodes[i + 1])):
            return False
    return True


def bag_match(expected, actual, eq):
    """Whether two sequences are equal as bags under `eq` (an equivalence, so a greedy
    match is exact)."""
    if len(expected) != len(actual):
        return False
    used = [False] * len(actual)
    for x in expected:
        for j, y in enumerate(actual):
            if not used[j] and eq(x, y):
                used[j] = True
                break
        else:
            return False
    return True
