Feature: Database Persistence
  As a developer
  I want to ensure that my data survives a server restart
  So that I can use Graphbuff for persistent applications

  Scenario: Data persists across restarts
    Given the server is running with persistent storage in "test_data"
    And I have created a node with name "PersistentAlice"
    When I restart the server
    And I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n {name: 'PersistentAlice'}) RETURN n.name as name"
      }
      """
    Then the response status code should be 200
    And the response JSON should have:
      | path            | value           |
      | rows[0][0]      | PersistentAlice |
