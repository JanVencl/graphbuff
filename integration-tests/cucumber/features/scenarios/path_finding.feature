Feature: Path Finding
  As a developer
  I want to find paths between nodes
  So that I can analyze graph connectivity

  Background:
    Given a fresh database
    And I have created the following graph:
      """
      CREATE (a:City {name: 'A'}), (b:City {name: 'B'}), (c:City {name: 'C'}), (d:City {name: 'D'}), (e:City {name: 'E'})
      CREATE (a)-[:ROAD]->(b), (b)-[:ROAD]->(c), (c)-[:ROAD]->(d)
      CREATE (a)-[:ROAD]->(e), (e)-[:ROAD]->(d)
      """

  Scenario: Shortest path between two nodes
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (a:City {name: 'A'}), (d:City {name: 'D'}) MATCH p = shortestPath((a)-[:ROAD*]->(d)) RETURN length(p) as len"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 2     |

  Scenario: All shortest paths
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (a:City {name: 'A'}), (d:City {name: 'D'}) MATCH p = allShortestPaths((a)-[:ROAD*]->(d)) RETURN count(p) as count"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path       | value |
      | rows[0][0] | 1     |
