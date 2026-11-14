Feature: LOAD CSV
  As a developer
  I want to import data from CSV files
  So that I can bulk load data into the graph

#  Scenario: Load simple CSV file
#    Given the server is started with import directory "test_import"
#    And a file "people.csv" in the import directory with content:
#      """
#      name,age
#      Alice,30
#      Bob,25
#      """
#    When I send a POST request to "/query" with JSON:
#      """
#      {
#        "query": "LOAD CSV WITH HEADERS FROM 'file:///people.csv' AS row CREATE (:Person {name: row.name, age: toInteger(row.age)})"
#      }
#      """
#    Then the response status code should be 200
#    When I send a POST request to "/query" with JSON:
#      """
#      {
#        "query": "MATCH (p:Person) RETURN p.name ORDER BY p.name"
#      }
#      """
#    Then the response JSON should have:
#      | path       | value |
#      | rows[0][0] | Alice |
#      | rows[1][0] | Bob   |
#
#  Scenario: Load CSV without headers
#    Given the server is started with import directory "test_import"
#    And a file "data.csv" in the import directory with content:
#      """
#      Alice,30
#      Bob,25
#      """
#    When I send a POST request to "/query" with JSON:
#      """
#      {
#        "query": "LOAD CSV FROM 'file:///data.csv' AS row CREATE (:Person {name: row[0], age: toInteger(row[1])})"
#      }
#      """
#    Then the response status code should be 200
#    When I send a POST request to "/query" with JSON:
#      """
#      {
#        "query": "MATCH (p:Person) RETURN p.name ORDER BY p.name"
#      }
#      """
#    Then the response JSON should have:
#      | path       | value |
#      | rows[0][0] | Alice |
#      | rows[1][0] | Bob   |
