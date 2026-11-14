Feature: Data Types
  As a developer
  I want to ensure the engine correctly handles various Cypher data types
  So that I can store and retrieve diverse information

  Scenario: Boolean and Numeric types
    Given a fresh database
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (n:Types {bool: true, int: 42, float: 3.14, string: 'hello'}) RETURN n.bool, n.int, n.float, n.string"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | rows[0][0]      | True    |
      | rows[0][1]      | 42      |
      | rows[0][2]      | 3.14    |
      | rows[0][3]      | hello   |

  Scenario: Null values
    Given a fresh database
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (n:Person {name: 'NoEmail'}) RETURN n.name, n.email"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value   |
      | rows[0][0]      | NoEmail |
      | rows[0][1]      | None    |
