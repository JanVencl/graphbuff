Feature: Extended Aggregations
  As a developer
  I want to use extended aggregation functions like sum, avg, min, and max
  So that I can perform numerical analysis on my graph data

  Background:
    Given a fresh database
    And I have created the following graph:
      """
      CREATE (:Product {name: 'A', price: 10.0, stock: 5})
      CREATE (:Product {name: 'B', price: 20.0, stock: 10})
      CREATE (:Product {name: 'C', price: 30.0, stock: 15})
      CREATE (:Product {name: 'D', price: 40.0, stock: 20})
      """

  Scenario: Sum aggregation
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Product) RETURN sum(p.price) as total_price, sum(p.stock) as total_stock"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 100.0 |
      | rows[0][1] | 50    |

  Scenario: Avg aggregation
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Product) RETURN avg(p.price) as avg_price, avg(p.stock) as avg_stock"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 25.0  |
      | rows[0][1] | 12.5  |

  Scenario: Min and Max aggregation
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Product) RETURN min(p.price) as min_price, max(p.price) as max_max_price"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 10.0  |
      | rows[0][1] | 40.0  |

  Scenario: Aggregations with nulls
    Given I have created the following graph:
      """
      CREATE (:Product {name: 'E'})
      """
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (p:Product) RETURN count(p.price), sum(p.price), avg(p.price)"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 4     |
      | rows[0][1] | 100.0 |
      | rows[0][2] | 25.0  |
