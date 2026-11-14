from behave import when, then, given
import requests
import json

@when('I send a GET request to "{endpoint}"')
def step_impl(context, endpoint):
    context.response = requests.get(context.base_url + endpoint)

@when('I send a POST request to "{endpoint}" with JSON:')
def step_impl(context, endpoint):
    try:
        payload = json.loads(context.text)
        context.response = requests.post(context.base_url + endpoint, json=payload)
    except json.JSONDecodeError:
        # If it's not valid JSON, send it as raw data with application/json header
        context.response = requests.post(
            context.base_url + endpoint, 
            data=context.text, 
            headers={"Content-Type": "application/json"}
        )

@then('the response status code should be {status_code:d}')
def step_impl(context, status_code):
    assert context.response.status_code == status_code, \
        f"Expected {status_code}, got {context.response.status_code}. Body: {context.response.text}"

@then('the response body should contain JSON:')
def step_impl(context):
    expected_json = json.loads(context.text)
    actual_json = context.response.json()
    # If stats is in actual but not in expected, remove it for comparison
    if 'stats' in actual_json and 'stats' not in expected_json:
        del actual_json['stats']
    assert actual_json == expected_json, f"Expected {expected_json}, got {actual_json}"

@then('the response body should contain a list of databases')
def step_impl(context):
    actual_json = context.response.json()
    assert "databases" in actual_json
    assert isinstance(actual_json["databases"], list)

@then('the response body should contain a field "{field}"')
def step_impl(context, field):
    actual_json = context.response.json()
    assert field in actual_json, f"Field {field} not found in response: {actual_json}"

@given('the server is started with import directory "{import_dir}"')
def step_impl(context, import_dir):
    stop_server(context)
    # Ensure import directory exists
    project_root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
    full_import_dir = os.path.join(project_root, import_dir)
    os.makedirs(full_import_dir, exist_ok=True)
    context.import_dir = full_import_dir
    start_server(context, args=["--import-dir", full_import_dir])

@given('a file "{filename}" in the import directory with content')
@given('a file "{filename}" in the import directory with content:')
def step_impl(context, filename):
    file_path = os.path.join(context.import_dir, filename)
    with open(file_path, 'w', encoding='utf-8') as f:
        f.write(context.text)

@then('the response JSON should have:')
def step_impl(context):
    actual_json = context.response.json()
    for row in context.table:
        path = row['path']
        expected_value = row['value']
        
        # Simple path evaluator for things like "columns[0]" or "rows[0][0]"
        value = actual_json
        # Normalize path: Replace '][' and ']' with '.' then split by '.'
        # Example: rows[0][0] -> rows.0.0
        normalized_path = path.replace('][', '.').replace('[', '.').replace(']', '')
        parts = normalized_path.split('.')
        
        for part in parts:
            if not part: continue
            if isinstance(value, list):
                try:
                    value = value[int(part)]
                except (IndexError, ValueError) as e:
                    raise AssertionError(f"Failed to access index {part} in list at path {path}. Value so far: {value}") from e
            elif isinstance(value, dict):
                try:
                    value = value[part]
                except KeyError as e:
                    raise AssertionError(f"Failed to find key '{part}' in dict at path {path}. Available keys: {list(value.keys())}") from e
            else:
                raise AssertionError(f"Cannot traverse into non-collection type {type(value)} at part '{part}' of path {path}")
        
        assert str(value) == expected_value, f"At path {path}, expected {expected_value}, got {value}"

import os
import shutil
from environment import start_server, stop_server

@given('the server is running with persistent storage in "{data_dir}"')
def step_impl(context, data_dir):
    stop_server(context)
    if os.path.exists(data_dir):
        shutil.rmtree(data_dir)
    os.makedirs(data_dir)
    context.data_dir = data_dir
    start_server(context, ["--data", data_dir, "--persistent"])

@when('I restart the server')
def step_impl(context):
    stop_server(context)
    if hasattr(context, 'data_dir'):
        args = ["--data", context.data_dir, "--persistent"]
    else:
        args = []
        # If we were using a data_dir before, we should clear it if we are now starting fresh?
        # No, the logic in environment.py handles it.
    start_server(context, args)

@given('a fresh database')
@when('a fresh database')
def step_impl(context):
    if hasattr(context, 'data_dir'):
        stop_server(context)
        if os.path.exists(context.data_dir):
            shutil.rmtree(context.data_dir)
        os.makedirs(context.data_dir)
        args = ["--data", context.data_dir, "--persistent"]
        start_server(context, args)
    else:
        # For in-memory, we try to clear it with a query first
        # Clear data
        requests.post(context.base_url + "/query", json={"query": "MATCH (n) DETACH DELETE n"})
        
        # Clear constraints
        resp = requests.post(context.base_url + "/query", json={"query": "SHOW CONSTRAINTS"})
        if resp.status_code == 200:
            data = resp.json()
            if "rows" in data:
                for row in data["rows"]:
                    constraint_name = row[0]
                    requests.post(context.base_url + "/query", json={"query": f"DROP CONSTRAINT {constraint_name}"})

        # Clear indexes
        resp = requests.post(context.base_url + "/query", json={"query": "SHOW INDEXES"})
        if resp.status_code == 200:
            data = resp.json()
            if "rows" in data:
                for row in data["rows"]:
                    index_name = row[0]
                    requests.post(context.base_url + "/query", json={"query": f"DROP INDEX {index_name}"})

        # And also restart just in case
        stop_server(context)
        start_server(context)

@given('I have created a node with name "{name}"')
@when('I have created a node with name "{name}"')
def step_impl(context, name):
    payload = {"query": f"CREATE (n {{name: '{name}'}})"}
    resp = requests.post(context.base_url + "/query", json=payload)
    assert resp.status_code == 200

@given('I have created the following graph')
@when('I have created the following graph')
@then('I have created the following graph')
@given('I have created the following graph:')
@when('I have created the following graph:')
@then('I have created the following graph:')
def step_impl(context):
    payload = {"query": context.text}
    resp = requests.post(context.base_url + "/query", json=payload)
    assert resp.status_code == 200, f"Query failed with {resp.status_code}: {resp.text}"
