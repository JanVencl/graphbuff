Feature: Functions
  As a developer
  I want to use built-in Cypher functions
  So that I can transform and analyze data

  Background:
    Given a fresh database

  Scenario: String functions
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "RETURN toUpper('hello') as u, toLower('WORLD') as l, reverse('abc') as r"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | HELLO |
      | rows[0][1] | world |
      | rows[0][2] | cba   |

  Scenario: List functions
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "RETURN size([1, 2, 3]) as s, range(1, 3) as r"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value     |
      | rows[0][0] | 3         |
      | rows[0][1] | [1, 2, 3] |

  Scenario: Math functions
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "RETURN abs(-5) as a, ceil(4.2) as c, floor(4.8) as f, round(4.5) as r"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 5     |
      | rows[0][1] | 5.0   |
      | rows[0][2] | 4.0   |
      | rows[0][3] | 5.0   |
