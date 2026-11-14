Feature: Regex Support
  As a developer
  I want to use the regex operator (=~) in Cypher queries
  So that I can perform complex string matching

  Background:
    Given a fresh database
    And I have created the following graph:
      """
      CREATE (:Person {name: 'Alice', email: 'alice@example.com'})
      CREATE (:Person {name: 'Bob', email: 'bob@work.net'})
      CREATE (:Person {name: 'Charlie', email: 'charlie@gmail.com'})
      CREATE (:Person {name: 'David', email: 'david123@example.com'})
      """

  Scenario: Basic regex matching
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) WHERE p.name =~ 'Al.*' RETURN p.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Alice |

  Scenario: Regex matching with case sensitivity
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) WHERE p.name =~ 'alice' RETURN count(p)"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 0     |

  Scenario: Regex matching for email patterns
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) WHERE p.email =~ '.*@example\\\\.com' RETURN p.name ORDER BY p.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | Alice |
      | rows[1][0] | David |

  Scenario: Regex with character classes
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Person) WHERE p.email =~ '.*[0-9].*@.*' RETURN p.name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | David |
