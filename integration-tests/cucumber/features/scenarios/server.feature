Feature: Graphbuff Server API
  As a developer
  I want to test the Graphbuff server via HTTP
  So that I can ensure it correctly handles Cypher queries

  Scenario: Health check
    When I send a GET request to "/health"
    Then the response status code should be 200
    And the response body should contain JSON:
      """
      {"status": "ok"}
      """

  Scenario: List built-in procedures
    Given a fresh database
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CALL dbms.procedures()"
      }
      """
    Then the response status code should be 200
    And the response body should contain a field "columns"
    And the response JSON should have:
      | path       | value |
      | columns[0] | name  |

  Scenario: Use db.labels procedure
    Given a fresh database
    And I have created the following graph:
      """
      CREATE (:Person), (:Company), (:Person)
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CALL db.labels() YIELD label RETURN label ORDER BY label"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value   |
      | rows[0][0] | Company |
      | rows[1][0] | Person  |

  Scenario: Run a simple Cypher query
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (n:Person {name: 'Alice'}) RETURN n.name as name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | columns[0]      | name    |
      | rows[0][0]      | Alice   |

  Scenario: Query a node that exists
    Given I have created a node with name "Bob"
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n {name: 'Bob'}) RETURN n.name as name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | rows[0][0]      | Bob     |
