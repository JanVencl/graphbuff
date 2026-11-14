Feature: Planner Features
  As a developer
  I want to use EXPLAIN and PROFILE to understand query execution
  So that I can optimize my queries

  Background:
    Given a fresh database
    And I have created the following graph:
      """
      CREATE (:Person {name: 'Alice', age: 30})
      CREATE (:Person {name: 'Bob', age: 25})
      """

  Scenario: EXPLAIN query
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "EXPLAIN MATCH (p:Person) WHERE p.age > 20 RETURN p.name"
      }
      """
    Then the response status code should be 200
    And the response body should contain a field "plan"
    And the response JSON should have:
      | path                 | value  |
      | plan.lines[0].operator | Select |

  Scenario: PROFILE query
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "PROFILE MATCH (p:Person) WHERE p.age > 20 RETURN p.name"
      }
      """
    Then the response status code should be 200
    And the response body should contain a field "plan"
    And the response JSON should have:
      | path                 | value  |
      | rows[0][0]           | Alice  |
      | rows[1][0]           | Bob    |
      | plan.lines[0].operator | Select |
      | plan.profiled        | True   |
