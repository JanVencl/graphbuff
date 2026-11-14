Feature: CRUD Operations
  As a developer
  I want to use MERGE, SET, REMOVE, and DELETE
  So that I can manage graph data effectively

  Background:
    Given a fresh database

  Scenario: MERGE creates a node if it does not exist
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MERGE (n:Person {name: 'Alice'}) RETURN n.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Alice |
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n:Person) RETURN count(n)"
      }
      """
    Then the response JSON should have:
      | path       | value |
      | rows[0][0] | 1     |

  Scenario: MERGE matches an existing node
    And I have created the following graph:
      """
      CREATE (:Person {name: 'Alice', age: 30})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MERGE (n:Person {name: 'Alice'}) SET n.found = true RETURN n.age, n.found"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 30    |
      | rows[0][1] | True  |
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n:Person) RETURN count(n)"
      }
      """
    Then the response JSON should have:
      | path       | value |
      | rows[0][0] | 1     |

  Scenario: SET and REMOVE properties
    And I have created the following graph:
      """
      CREATE (p:Person {name: 'Bob', age: 25})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person {name: 'Bob'}) SET p.age = 26, p.city = 'New York' RETURN p.age, p.city"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value    |
      | rows[0][0] | 26       |
      | rows[0][1] | New York |
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person {name: 'Bob'}) REMOVE p.age RETURN p.name, p.age"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Bob   |
      | rows[0][1] | None  |

  Scenario: DELETE a node
    And I have created the following graph:
      """
      CREATE (:Person {name: 'Alice'})
      CREATE (:Person {name: 'Bob'})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person {name: 'Alice'}) DELETE p"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) RETURN count(p)"
      }
      """
    Then the response JSON should have:
      | path       | value |
      | rows[0][0] | 1     |

  Scenario: DELETE a relationship
    And I have created the following graph:
      """
      CREATE (a:Person {name: 'Alice'})-[r:KNOWS]->(b:Person {name: 'Bob'})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH ()-[r:KNOWS]->() DELETE r"
      }
      """
    Then the response status code should be 200
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH ()-[r:KNOWS]->() RETURN count(r)"
      }
      """
    Then the response JSON should have:
      | path       | value |
      | rows[0][0] | 0     |
