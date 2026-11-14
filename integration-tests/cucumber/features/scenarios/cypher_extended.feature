Feature: Extended Cypher Queries
  As a developer
  I want to test more complex Cypher queries
  So that I can ensure the engine correctly handles various graph patterns

  Scenario: Create and match relationships
    Given a fresh database
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (a:Person {name: 'Alice'})-[:KNOWS {since: 2020}]->(b:Person {name: 'Bob'}) RETURN a.name, b.name"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b:Person) RETURN b.name as friend, r.since as since"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | rows[0][0]      | Bob     |
      | rows[0][1]      | 2020    |

  Scenario: Aggregations
    Given a fresh database
    And I have created a node with name "User1"
    And I have created a node with name "User2"
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n) RETURN count(n) as count"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | rows[0][0]      | 2       |

  Scenario: Complex graph patterns
    Given a fresh database
    And I have created a node with name "A"
    And I have created a node with name "B"
    And I have created a node with name "C"
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (a {name: 'A'}), (b {name: 'B'}), (c {name: 'C'}) CREATE (a)-[:LINK]->(b)-[:LINK]->(c) RETURN a.name, b.name, c.name"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (a)-[:LINK]->(b)-[:LINK]->(c) RETURN a.name, c.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | rows[0][0]      | A       |
      | rows[0][1]      | C       |

  Scenario: Multiple aggregations
    Given a fresh database
    And I have created a node with name "User1"
    And I have created a node with name "User2"
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n) RETURN count(n) as total, collect(n.name) as names"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value           |
      | rows[0][0]      | 2               |
      | rows[0][1][0]   | User1           |
      | rows[0][1][1]   | User2           |
