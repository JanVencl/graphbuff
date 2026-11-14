Feature: Error Handling
  As a developer
  I want to ensure the server correctly handles invalid requests
  So that I can provide better feedback to users

  Scenario: Invalid Cypher query
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "NOT A QUERY"
      }
      """
    Then the response status code should be 400

  Scenario: Non-existent endpoint
    When I send a GET request to "/non-existent"
    Then the response status code should be 404

  Scenario: Invalid JSON payload
    When I send a POST request to "/query" with JSON:
      """
      {
        "query": "MATCH (n) RETURN n",
        "missing_bracket": 
      }
      """
    Then the response status code should be 400
