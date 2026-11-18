import json
import os
import shutil

from behave import given, then, when

from environment import restart_server

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))


def _query(context, query):
    return context.server.http.post(context.base_url + "/query", json={"query": query})


# ---- HTTP requests ----------------------------------------------------------------------

@when('I send a GET request to "{endpoint}"')
def get_request(context, endpoint):
    context.response = context.server.http.get(context.base_url + endpoint)


@when('I send a POST request to "{endpoint}" with JSON:')
def post_request(context, endpoint):
    # Invalid JSON is sent as it is, to test how the server rejects it.
    context.response = context.server.http.post(context.base_url + endpoint, data=context.text.encode(),
                                     headers={"Content-Type": "application/json"})


@then('the response status code should be {status_code:d}')
def status_code_is(context, status_code):
    assert context.response.status_code == status_code, \
        f"Expected {status_code}, got {context.response.status_code}. Body: {context.response.text}"


@then('the response body should contain JSON:')
def body_is(context):
    expected = json.loads(context.text)
    actual = context.response.json()
    if "stats" in actual and "stats" not in expected:
        del actual["stats"]
    assert actual == expected, f"Expected {expected}, got {actual}"


@then('the response body should contain a list of databases')
def body_lists_databases(context):
    actual = context.response.json()
    assert isinstance(actual.get("databases"), list), actual


@then('the response body should contain a field "{field}"')
def body_has_field(context, field):
    actual = context.response.json()
    assert field in actual, f"Field {field} not found in response: {actual}"


@then('the response JSON should have:')
def body_values(context):
    """Checks `path | value` rows; a path is like `rows[0][0]` or `stats.nodes_created`."""
    actual = context.response.json()
    for row in context.table:
        path, expected = row["path"], row["value"]
        value = actual
        for part in path.replace("][", ".").replace("[", ".").replace("]", "").split("."):
            if not part:
                continue
            try:
                value = value[int(part)] if isinstance(value, list) else value[part]
            except (IndexError, KeyError, ValueError, TypeError) as e:
                raise AssertionError(f"no `{part}` at path {path} in {value!r}") from e
        assert str(value) == expected, f"At path {path}, expected {expected}, got {value}"


# ---- server setup -----------------------------------------------------------------------

@given('the server is started with import directory "{import_dir}"')
def server_with_import_dir(context, import_dir):
    context.import_dir = os.path.join(ROOT, import_dir)
    os.makedirs(context.import_dir, exist_ok=True)
    restart_server(context, ["--import-dir", context.import_dir])


@given('a file "{filename}" in the import directory with content')
@given('a file "{filename}" in the import directory with content:')
def import_file(context, filename):
    with open(os.path.join(context.import_dir, filename), "w", encoding="utf-8") as f:
        f.write(context.text)


@given('the server is running with persistent storage in "{data_dir}"')
def persistent_server(context, data_dir):
    if os.path.exists(data_dir):
        shutil.rmtree(data_dir)
    os.makedirs(data_dir)
    context.data_dir = data_dir
    restart_server(context, ["--data", data_dir, "--persistent"])


@when('I restart the server')
def restart(context):
    data_dir = getattr(context, "data_dir", None)
    restart_server(context, ["--data", data_dir, "--persistent"] if data_dir else [])


@given('a fresh database')
@when('a fresh database')
def fresh_database(context):
    data_dir = getattr(context, "data_dir", None)
    if data_dir:
        shutil.rmtree(data_dir, ignore_errors=True)
        os.makedirs(data_dir)
        restart_server(context, ["--data", data_dir, "--persistent"])
    else:
        restart_server(context)


# ---- data -------------------------------------------------------------------------------

@given('I have created a node with name "{name}"')
@when('I have created a node with name "{name}"')
def create_named_node(context, name):
    resp = context.server.http.post(context.base_url + "/query",
                         json={"query": "CREATE ({name: $name})", "params": {"name": name}})
    assert resp.status_code == 200, resp.text


@given('I have created the following graph')
@when('I have created the following graph')
@then('I have created the following graph')
@given('I have created the following graph:')
@when('I have created the following graph:')
@then('I have created the following graph:')
def create_graph(context):
    resp = _query(context, context.text)
    assert resp.status_code == 200, f"Query failed with {resp.status_code}: {resp.text}"
