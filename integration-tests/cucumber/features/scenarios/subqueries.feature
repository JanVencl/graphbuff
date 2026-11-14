Feature: Subqueries
  As a developer
  I want to use subqueries in Cypher
  So that I can write more expressive queries

  Background:
    Given a fresh database
    And I have created the following graph:
      """
      CREATE (:Person {name: 'Alice', age: 30})-[:KNOWS]->(:Person {name: 'Bob', age: 25})
      CREATE (:Person {name: 'Charlie', age: 35})
      """

  Scenario: EXISTS subquery
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) WHERE EXISTS { (p)-[:KNOWS]->() } RETURN p.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Alice |

  Scenario: COUNT subquery
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) RETURN p.name, COUNT { (p)-[:KNOWS]->() } as friends"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Alice |
      | rows[0][1] | 1     |
      | rows[1][0] | Bob   |
      | rows[1][1] | 0     |

  Scenario: CALL subquery
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) CALL { WITH p RETURN p.age * 2 as double_age } RETURN p.name, double_age"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Alice |
      | rows[0][1] | 60    |
