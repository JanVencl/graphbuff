// A tiny movie graph. Run with: cargo run --release -p graphbuff-cli -- -i examples/movies.cypher
// It is also built into the binaries: graphbuff --demo movies, graphbuff-server --demo movies
CREATE (keanu:Person {name: 'Keanu Reeves', born: 1964}),
       (carrie:Person {name: 'Carrie-Anne Moss', born: 1967}),
       (lana:Person {name: 'Lana Wachowski', born: 1965}),
       (hugo:Person {name: 'Hugo Weaving', born: 1960}),
       (matrix:Movie {title: 'The Matrix', released: 1999}),
       (reloaded:Movie {title: 'The Matrix Reloaded', released: 2003}),
       (johnwick:Movie {title: 'John Wick', released: 2014}),
       (keanu)-[:ACTED_IN {role: 'Neo'}]->(matrix),
       (keanu)-[:ACTED_IN {role: 'Neo'}]->(reloaded),
       (keanu)-[:ACTED_IN {role: 'John Wick'}]->(johnwick),
       (carrie)-[:ACTED_IN {role: 'Trinity'}]->(matrix),
       (carrie)-[:ACTED_IN {role: 'Trinity'}]->(reloaded),
       (hugo)-[:ACTED_IN {role: 'Agent Smith'}]->(matrix),
       (lana)-[:DIRECTED]->(matrix),
       (lana)-[:DIRECTED]->(reloaded);

// Who acted with Keanu, and how often?
MATCH (k:Person {name: 'Keanu Reeves'})-[:ACTED_IN]->(m)<-[:ACTED_IN]-(co)
RETURN co.name AS coActor, count(m) AS movies, collect(m.title) AS titles
ORDER BY movies DESC, coActor;

// Movies per decade
MATCH (m:Movie)
RETURN (m.released / 10) * 10 AS decade, count(*) AS movies
ORDER BY decade;

// Everyone within four hops of Hugo Weaving
MATCH (h:Person {name: 'Hugo Weaving'})-[*1..4]-(other:Person)
WHERE other <> h
RETURN DISTINCT other.name AS reachable ORDER BY reachable;
