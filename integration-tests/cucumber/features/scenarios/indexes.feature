Feature: Indexes
  As a developer
  I want to use various types of indexes
  So that I can improve query performance

  Background:
    Given a fresh database

  Scenario: Composite index on node properties
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE INDEX FOR (n:Person) ON (n.firstname, n.surname)"
      }
      """
    Then the response status code should be 200
    And I have created the following graph:
      """
      CREATE (:Person {firstname: 'John', surname: 'Doe', age: 30})
      CREATE (:Person {firstname: 'Jane', surname: 'Doe', age: 25})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person {firstname: 'John', surname: 'Doe'}) RETURN p.age"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 30    |

  Scenario: Relationship property index
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE INDEX FOR ()-[r:KNOWS]-() ON (r.since)"
      }
      """
    Then the response status code should be 200
    And I have created the following graph:
      """
      CREATE (a:Person {name: 'Alice'})
      CREATE (b:Person {name: 'Bob'})
      CREATE (a)-[:KNOWS {since: 2020}]->(b)
      CREATE (a)-[:KNOWS {since: 2022}]->(c:Person {name: 'Charlie'})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH ()-[r:KNOWS {since: 2022}]->(p) RETURN p.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value   |
      | rows[0][0] | Charlie |

  Scenario: Show indexes
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE INDEX person_idx FOR (n:Person) ON (n.email)"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "SHOW INDEXES"
      }
      """
    Then the response status code should be 200
    And the response body should contain JSON:
      """
      {
        "columns": ["name", "entityType", "labelOrType", "properties"],
        "rows": [
          ["person_idx", "NODE", "Person", ["email"]]
        ]
      }
      """
