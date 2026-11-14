Feature: Temporal and Spatial Data Types
  As a developer
  I want to ensure the engine correctly handles temporal and spatial Cypher types over the HTTP API
  So that I can use time and location in my applications

  Background:
    Given a fresh database

  Scenario: Basic Temporal types creation and retrieval
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (n:Event {d: date('2024-01-01'), t: localtime('12:30:00'), dt: datetime('2024-01-01T12:30:00Z')}) RETURN n.d, n.t, n.dt"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value                     |
      | rows[0][0] | 2024-01-01                |
      | rows[0][1] | 12:30:00                  |
      | rows[0][2] | 2024-01-01T12:30:00Z      |

  Scenario: Temporal arithmetic
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "RETURN date('2024-01-01') + duration('P1M') as next_month"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value      |
      | rows[0][0] | 2024-02-01 |

  Scenario: Spatial types (Points)
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "CREATE (p:Place {loc: point({x: 1.0, y: 2.0})}) RETURN p.loc"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path              | value |
      | rows[0][0][$type] | point |
      | rows[0][0][x]     | 1.0   |
      | rows[0][0][y]     | 2.0   |

  Scenario: Spatial distance
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "WITH point({x: 0, y: 0}) as p1, point({x: 3, y: 4}) as p2 RETURN point.distance(p1, p2) as dist"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 5.0   |
