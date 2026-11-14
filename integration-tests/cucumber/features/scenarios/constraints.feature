Feature: Constraints
  As a developer
  I want to ensure the engine correctly enforces schema constraints
  So that I can maintain data integrity

  Background:
    Given a fresh database

  Scenario: Unique constraint on node property
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS UNIQUE"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:User {email: 'alice@example.com'})"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:User {email: 'alice@example.com'})"
      }
      """
    Then the response status code should be 409

  Scenario: Not null constraint on node property
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE CONSTRAINT FOR (u:User) REQUIRE u.name IS NOT NULL"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:User {name: 'Bob'})"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:User {age: 30})"
      }
      """
    Then the response status code should be 409

  Scenario: Node key constraint
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE CONSTRAINT FOR (p:Person) REQUIRE (p.firstname, p.surname) IS NODE KEY"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:Person {firstname: 'John', surname: 'Doe'})"
      }
      """
    Then the response status code should be 200
    # Violation 1: Duplicate key
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:Person {firstname: 'John', surname: 'Doe'})"
      }
      """
    Then the response status code should be 409
    # Violation 2: Missing property
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (:Person {firstname: 'Jane'})"
      }
      """
    Then the response status code should be 409
