package dev.graphbuff.smoke;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;
import java.time.OffsetDateTime;
import java.time.OffsetTime;
import java.time.ZoneOffset;
import java.time.ZonedDateTime;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.stream.Collectors;
import java.util.stream.IntStream;
import java.util.stream.StreamSupport;

import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.neo4j.driver.AuthTokens;
import org.neo4j.driver.Bookmark;
import org.neo4j.driver.Driver;
import org.neo4j.driver.GraphDatabase;
import org.neo4j.driver.Query;
import org.neo4j.driver.QueryConfig;
import org.neo4j.driver.Record;
import org.neo4j.driver.Result;
import org.neo4j.driver.RoutingControl;
import org.neo4j.driver.Session;
import org.neo4j.driver.SessionConfig;
import org.neo4j.driver.Transaction;
import org.neo4j.driver.TransactionConfig;
import org.neo4j.driver.Value;
import org.neo4j.driver.Values;
import org.neo4j.driver.async.AsyncSession;
import org.neo4j.driver.async.ResultCursor;
import org.neo4j.driver.exceptions.AuthenticationException;
import org.neo4j.driver.exceptions.ClientException;
import org.neo4j.driver.exceptions.FatalDiscoveryException;
import org.neo4j.driver.summary.QueryType;
import org.neo4j.driver.summary.ResultSummary;
import org.neo4j.driver.summary.SummaryCounters;
import org.neo4j.driver.types.IsoDuration;
import org.neo4j.driver.types.Node;
import org.neo4j.driver.types.Path;
import org.neo4j.driver.types.Point;
import org.neo4j.driver.types.Relationship;

/** Smoke suite: the official Neo4j Java driver against graphbuff-server over Bolt. */
class DriverSmokeTest {
    static Server server;
    static Driver driver;

    @BeforeAll
    static void start() throws Exception {
        server = Server.get();
        driver = GraphDatabase.driver(server.boltUri(), AuthTokens.basic("neo4j", Server.TOKEN));
    }

    @AfterAll
    static void stop() {
        driver.close();
    }

    @BeforeEach
    void clean() {
        driver.executableQuery("MATCH (n) DETACH DELETE n").execute();
    }

    private static Session session() {
        return driver.session();
    }

    // --- connection -----------------------------------------------------------

    @Test
    void verifyConnectivityAndServerInfo() {
        driver.verifyConnectivity();
        var info = driver.executableQuery("RETURN 1").execute().summary().server();
        assertTrue(info.agent().startsWith("Neo4j/"), info.agent());
        assertNotNull(info.protocolVersion());
    }

    @Test
    void routingScheme() {
        try (Driver d = GraphDatabase.driver(server.neo4jUri(), AuthTokens.basic("neo4j", Server.TOKEN))) {
            d.verifyConnectivity();
            var read = d.executableQuery("RETURN 1 AS x")
                    .withConfig(QueryConfig.builder().withRouting(RoutingControl.READ).build())
                    .execute();
            assertEquals(1, read.records().get(0).get("x").asInt());
            var write = d.executableQuery("RETURN 2 AS x").execute();
            assertEquals(2, write.records().get(0).get("x").asInt());
        }
    }

    @Test
    void wrongPassword() {
        try (Driver d = GraphDatabase.driver(server.boltUri(), AuthTokens.basic("neo4j", "wrong"))) {
            assertThrows(AuthenticationException.class, d::verifyConnectivity);
        }
    }

    @Test
    void bearerAuth() {
        try (Driver d = GraphDatabase.driver(server.boltUri(), AuthTokens.bearer(Server.TOKEN))) {
            d.verifyConnectivity();
        }
    }

    // --- values ---------------------------------------------------------------

    @Test
    void scalarValues() {
        try (Session s = session()) {
            Record r = s.run("RETURN 1 AS i, -9007199254740993 AS big, 1.5 AS f, 'héllo' AS s, true AS b,"
                    + " null AS n, [1, 'a', null] AS l, {k: [1, {x: 2}]} AS m").single();
            assertEquals(List.of("i", "big", "f", "s", "b", "n", "l", "m"), r.keys());
            assertEquals(1L, r.get("i").asLong());
            assertEquals(-9007199254740993L, r.get("big").asLong());
            assertEquals(1.5, r.get("f").asDouble());
            assertEquals("héllo", r.get("s").asString());
            assertTrue(r.get("b").asBoolean());
            assertTrue(r.get("n").isNull());
            assertEquals(java.util.Arrays.asList(1L, "a", null), r.get("l").asList());
            assertEquals(Map.of("k", List.of(1L, Map.of("x", 2L))), r.get("m").asMap());
        }
    }

    @Test
    void parametersRoundTrip() {
        String longString = "x".repeat(70_000);
        List<Long> longList = IntStream.range(0, 300).mapToObj(i -> (long) i).toList();
        Map<String, Object> nested = Map.of("nested", Map.of("deep", List.of("a", 1L, 2.5)));
        try (Session s = session()) {
            Record r = s.run("RETURN $i AS i, $f AS f, $s AS s, $b AS b, $n AS n, $l AS l, $m AS m,"
                            + " $el AS el, $em AS em, $bytesLen AS bl",
                    Values.parameters("i", 1L << 62, "f", -0.25, "s", longString, "b", false, "n", null,
                            "l", longList, "m", nested, "el", List.of(), "em", Map.of(), "bytesLen", 3))
                    .single();
            assertEquals(1L << 62, r.get("i").asLong());
            assertEquals(-0.25, r.get("f").asDouble());
            assertEquals(longString, r.get("s").asString());
            assertFalse(r.get("b").asBoolean());
            assertTrue(r.get("n").isNull());
            assertEquals(longList, r.get("l").asList());
            assertEquals(nested, r.get("m").asMap());
            assertEquals(List.of(), r.get("el").asList());
            assertEquals(Map.of(), r.get("em").asMap());
        }
    }

    @Test
    void temporalValues() {
        try (Session s = session()) {
            Record r = s.run("RETURN date('2024-02-29') AS d, localtime('12:34:56.789') AS lt,"
                    + " time('12:00:00+02:00') AS t, localdatetime('2024-01-02T03:04:05') AS ldt,"
                    + " datetime('2024-01-02T03:04:05.123456789+01:00') AS dt,"
                    + " duration('P1Y2M3DT4H5M6.5S') AS dur").single();
            assertEquals(LocalDate.of(2024, 2, 29), r.get("d").asLocalDate());
            assertEquals(LocalTime.of(12, 34, 56, 789_000_000), r.get("lt").asLocalTime());
            assertEquals(OffsetTime.of(12, 0, 0, 0, ZoneOffset.ofHours(2)), r.get("t").asOffsetTime());
            assertEquals(LocalDateTime.of(2024, 1, 2, 3, 4, 5), r.get("ldt").asLocalDateTime());
            assertEquals(OffsetDateTime.of(2024, 1, 2, 3, 4, 5, 123_456_789, ZoneOffset.ofHours(1)),
                    r.get("dt").asOffsetDateTime());
            IsoDuration dur = r.get("dur").asIsoDuration();
            assertEquals(14, dur.months());
            assertEquals(3, dur.days());
            assertEquals(4 * 3600 + 5 * 60 + 6, dur.seconds());
            assertEquals(500_000_000, dur.nanoseconds());
        }
    }

    @Test
    void temporalParameters() {
        var dt = OffsetDateTime.of(2001, 2, 3, 4, 5, 6, 0, ZoneOffset.ofHours(-5));
        var utc = ZonedDateTime.of(2001, 2, 3, 4, 5, 6, 0, ZoneOffset.UTC);
        try (Session s = session()) {
            Record r = s.run("RETURN toString($d) AS d, toString($ldt) AS ldt, toString($dt) AS dt,"
                            + " toString($utc) AS utc, toString($lt) AS lt, toString($dur) AS dur,"
                            + " $dt.epochSeconds AS epoch, $dt AS same",
                    Values.parameters("d", LocalDate.of(2001, 2, 3),
                            "ldt", LocalDateTime.of(2001, 2, 3, 4, 5, 6),
                            "dt", dt, "utc", utc, "lt", LocalTime.of(7, 8, 9),
                            "dur", Values.isoDuration(0, 2, 30, 0).asIsoDuration()))
                    .single();
            assertEquals("2001-02-03", r.get("d").asString());
            assertEquals("2001-02-03T04:05:06", r.get("ldt").asString());
            assertEquals("2001-02-03T04:05:06-05:00", r.get("dt").asString());
            assertEquals("2001-02-03T04:05:06Z", r.get("utc").asString());
            assertEquals("07:08:09", r.get("lt").asString());
            assertEquals("P2DT30S", r.get("dur").asString());
            assertEquals(dt.toEpochSecond(), r.get("epoch").asLong());
            assertEquals(dt, r.get("same").asOffsetDateTime());
        }
    }

    @Test
    void spatialValues() {
        try (Session s = session()) {
            Record r = s.run("RETURN point({x: 1.5, y: -2}) AS c, point({x: 1, y: 2, z: 3}) AS c3,"
                    + " point({longitude: 14.42, latitude: 50.09}) AS g").single();
            Point c = r.get("c").asPoint();
            assertEquals(7203, c.srid());
            assertEquals(1.5, c.x());
            assertEquals(-2.0, c.y());
            Point c3 = r.get("c3").asPoint();
            assertEquals(9157, c3.srid());
            assertEquals(3.0, c3.z());
            Point g = r.get("g").asPoint();
            assertEquals(4326, g.srid());
            assertEquals(14.42, g.x());
            assertEquals(50.09, g.y());
            Record d = s.run("RETURN $p.x AS x, point.distance($p, point({x: 0, y: 0})) AS d",
                    Values.parameters("p", Values.point(7203, 3, 4))).single();
            assertEquals(3.0, d.get("x").asDouble());
            assertEquals(5.0, d.get("d").asDouble());
        }
    }

    @Test
    void graphValues() {
        try (Session s = session()) {
            Record r = s.run("CREATE p = (a:Person:Admin {name: 'Ann', age: 30})-[r:KNOWS {since: 2020}]->"
                    + "(b:Person {name: 'Bob'}) RETURN a, r, b, p").single();
            Node a = r.get("a").asNode();
            Relationship rel = r.get("r").asRelationship();
            Node b = r.get("b").asNode();
            Path p = r.get("p").asPath();
            assertEquals(Set.of("Person", "Admin"), toSet(a.labels()));
            assertTrue(a.hasLabel("Admin"));
            assertEquals(Map.of("name", "Ann", "age", 30L), a.asMap());
            assertEquals("KNOWS", rel.type());
            assertEquals(2020, rel.get("since").asInt());
            assertEquals(a.elementId(), rel.startNodeElementId());
            assertEquals(b.elementId(), rel.endNodeElementId());
            assertEquals(1, p.length());
            assertEquals(a.elementId(), p.start().elementId());
            assertEquals(b.elementId(), p.end().elementId());
            Record found = s.run("MATCH (n) WHERE elementId(n) = $id RETURN n.name AS name",
                    Values.parameters("id", b.elementId())).single();
            assertEquals("Bob", found.get("name").asString());
            Record byRel = s.run("MATCH ()-[x]->() WHERE elementId(x) = $id RETURN x.since AS since",
                    Values.parameters("id", rel.elementId())).single();
            assertEquals(2020, byRel.get("since").asInt());
        }
    }

    @Test
    void reverseRelationshipInPath() {
        try (Session s = session()) {
            s.run("CREATE (:A {n: 1})-[:R]->(:B {n: 2})").consume();
            Path p = s.run("MATCH p = (:B)<-[:R]-(:A) RETURN p").single().get("p").asPath();
            List<Long> ns = StreamSupport.stream(p.nodes().spliterator(), false)
                    .map(n -> n.get("n").asLong()).toList();
            assertEquals(List.of(2L, 1L), ns);
            Path.Segment seg = p.iterator().next();
            assertEquals(2L, seg.start().get("n").asLong());
            assertEquals(1L, seg.end().get("n").asLong());
            assertEquals(seg.end().elementId(), seg.relationship().startNodeElementId());
        }
    }

    // --- results --------------------------------------------------------------

    @Test
    void streamingWithSmallFetchSize() {
        try (Session s = driver.session(SessionConfig.builder().withFetchSize(7).build())) {
            List<Long> xs = s.run("UNWIND range(1, 1000) AS x RETURN x").list(rec -> rec.get("x").asLong());
            assertEquals(LongRange.of(1, 1000), xs);
        }
    }

    @Test
    void partiallyConsumedResults() {
        try (Session s = driver.session(SessionConfig.builder().withFetchSize(10).build())) {
            Result first = s.run("UNWIND range(1, 100) AS x RETURN x");
            assertEquals(1, first.next().get("x").asInt());
            assertEquals(42, s.run("RETURN 42 AS x").single().get("x").asInt());
        }
    }

    @Test
    void summaryCounters() {
        try (Session s = session()) {
            ResultSummary sum = s.run("CREATE (a:L {p: 1})-[:T]->(b:L), (c:M) SET c.q = 2 RETURN a").consume();
            SummaryCounters c = sum.counters();
            assertEquals(3, c.nodesCreated());
            assertEquals(1, c.relationshipsCreated());
            assertEquals(3, c.labelsAdded());
            assertEquals(2, c.propertiesSet());
            assertTrue(c.containsUpdates());
            assertEquals(QueryType.READ_WRITE, sum.queryType());
            SummaryCounters del = s.run("MATCH (n) DETACH DELETE n").consume().counters();
            assertEquals(3, del.nodesDeleted());
            assertEquals(1, del.relationshipsDeleted());
            assertEquals(QueryType.READ_ONLY, s.run("RETURN 1").consume().queryType());
        }
    }

    @Test
    void schemaCounters() {
        try (Session s = session()) {
            ResultSummary sum = s.run("CREATE INDEX smoke_idx IF NOT EXISTS FOR (n:Smoke) ON (n.k)").consume();
            assertEquals(1, sum.counters().indexesAdded());
            assertEquals(QueryType.SCHEMA_WRITE, sum.queryType());
            assertEquals(1, s.run("DROP INDEX smoke_idx").consume().counters().indexesRemoved());
        }
    }

    @Test
    void explainAndProfile() {
        try (Session s = session()) {
            ResultSummary explain = s.run("EXPLAIN MATCH (n) RETURN n").consume();
            assertTrue(explain.hasPlan());
            assertNotNull(explain.plan().operatorType());
            s.run("CREATE (:X), (:X)").consume();
            ResultSummary profile = s.run("PROFILE MATCH (n:X) RETURN n").consume();
            assertTrue(profile.hasProfile());
            assertEquals(2, profile.profile().records());
        }
    }

    @Test
    void executableQueryApi() {
        driver.executableQuery("CREATE (:Q {v: $v})").withParameters(Map.of("v", 7)).execute();
        var res = driver.executableQuery("MATCH (q:Q) RETURN q.v AS v").execute();
        assertEquals(List.of("v"), res.keys());
        assertEquals(7, res.records().get(0).get("v").asInt());
        assertNotNull(res.summary().database().name());
        long count = driver.executableQuery("MATCH (q:Q) RETURN q")
                .execute(Collectors.counting());
        assertEquals(1, count);
    }

    @Test
    void emptyAndSingleResults() {
        try (Session s = session()) {
            assertFalse(s.run("RETURN 1 AS x LIMIT 0").hasNext());
            assertThrows(org.neo4j.driver.exceptions.NoSuchRecordException.class,
                    () -> s.run("UNWIND [1, 2] AS x RETURN x").single());
            Value v = s.run("RETURN [1, 2, 3] AS l").single().get("l");
            assertEquals(3, v.size());
        }
    }

    // --- transactions ---------------------------------------------------------

    @Test
    void managedTransactions() {
        try (Session s = session()) {
            for (String name : List.of("a", "b")) {
                s.executeWriteWithoutResult(tx -> tx.run("CREATE (:Acc {name: $n, bal: 100})",
                        Values.parameters("n", name)).consume());
            }
            long total = s.executeRead(tx -> tx.run("MATCH (a:Acc) RETURN sum(a.bal) AS s")
                    .single().get("s").asLong());
            assertEquals(200, total);
        }
    }

    @Test
    void explicitCommitAndRollback() {
        try (Session s = session()) {
            try (Transaction tx = s.beginTransaction()) {
                tx.run("CREATE (:T {v: 1})").consume();
                assertEquals(1, tx.run("MATCH (t:T) RETURN count(t) AS c").single().get("c").asInt());
                tx.commit();
            }
            try (Transaction tx = s.beginTransaction()) {
                tx.run("CREATE (:T {v: 2})").consume();
                tx.rollback();
            }
            try (Transaction tx = s.beginTransaction()) {
                tx.run("CREATE (:T {v: 3})").consume();
                // Closed without commit: rolled back.
            }
            List<Long> vs = s.run("MATCH (t:T) RETURN t.v AS v ORDER BY v").list(r -> r.get("v").asLong());
            assertEquals(List.of(1L), vs);
        }
    }

    @Test
    void severalOpenResultsInOneTransaction() {
        try (Session s = driver.session(SessionConfig.builder().withFetchSize(2).build());
             Transaction tx = s.beginTransaction()) {
            Result r1 = tx.run("UNWIND range(1, 5) AS x RETURN x");
            Result r2 = tx.run("UNWIND range(10, 15) AS y RETURN y");
            assertEquals(LongRange.of(10, 15), r2.list(r -> r.get("y").asLong()));
            assertEquals(LongRange.of(1, 5), r1.list(r -> r.get("x").asLong()));
            tx.commit();
        }
    }

    @Test
    void transactionMetadataAndTimeoutConfig() {
        TransactionConfig cfg = TransactionConfig.builder()
                .withTimeout(java.time.Duration.ofSeconds(10))
                .withMetadata(Map.of("app", "smoke"))
                .build();
        try (Session s = session()) {
            assertEquals(1, s.run("CREATE (:Meta) RETURN 1 AS x", cfg).single().get("x").asInt());
            try (Transaction tx = s.beginTransaction(cfg)) {
                tx.run("RETURN 1").consume();
                tx.commit();
            }
        }
    }

    @Test
    void queryTimeout() {
        TransactionConfig cfg = TransactionConfig.builder()
                .withTimeout(java.time.Duration.ofMillis(200)).build();
        try (Session s = session()) {
            ClientException e = assertThrows(ClientException.class, () -> s.run(
                    "UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*)", cfg).consume());
            assertTrue(e.code().contains("Transaction"), e.code());
            assertEquals(1, s.run("RETURN 1 AS x").single().get("x").asInt());
        }
    }

    @Test
    void bookmarksChainSessions() {
        Set<Bookmark> bookmarks;
        try (Session s1 = session()) {
            s1.run("CREATE (:Bm {v: 1})").consume();
            bookmarks = s1.lastBookmarks();
        }
        assertFalse(bookmarks.isEmpty());
        try (Session s2 = driver.session(SessionConfig.builder().withBookmarks(bookmarks).build())) {
            assertEquals(1, s2.run("MATCH (b:Bm) RETURN count(b) AS c").single().get("c").asInt());
        }
    }

    @Test
    void concurrentSessions() throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(8);
        List<Future<?>> fs = new ArrayList<>();
        for (int i = 0; i < 8; i++) {
            int w = i;
            fs.add(pool.submit(() -> {
                try (Session s = session()) {
                    for (int j = 0; j < 20; j++) {
                        int jj = j;
                        s.executeWriteWithoutResult(tx -> tx.run("CREATE (:C {w: $w, j: $j})",
                                Values.parameters("w", w, "j", jj)).consume());
                    }
                }
            }));
        }
        for (Future<?> f : fs) {
            f.get(60, TimeUnit.SECONDS);
        }
        pool.shutdown();
        var res = driver.executableQuery("MATCH (c:C) RETURN count(c) AS n").execute();
        assertEquals(160, res.records().get(0).get("n").asInt());
    }

    @Test
    void asyncApi() throws Exception {
        AsyncSession s = driver.session(AsyncSession.class);
        try {
            CompletionStage<List<Long>> xs = s.runAsync("UNWIND range(1, 50) AS x RETURN x")
                    .thenCompose(ResultCursor::listAsync)
                    .thenApply(rs -> rs.stream().map(r -> r.get("x").asLong()).toList());
            assertEquals(LongRange.of(1, 50), xs.toCompletableFuture().get(30, TimeUnit.SECONDS));
            Long n = s.executeWriteAsync(tx -> tx.runAsync("CREATE (:Async) RETURN 5 AS n")
                            .thenCompose(ResultCursor::singleAsync)
                            .thenApply(r -> r.get("n").asLong()))
                    .toCompletableFuture().get(30, TimeUnit.SECONDS);
            assertEquals(5L, n);
        } finally {
            s.closeAsync().toCompletableFuture().get(30, TimeUnit.SECONDS);
        }
    }

    // --- errors ---------------------------------------------------------------

    @Test
    void syntaxError() {
        try (Session s = session()) {
            ClientException e = assertThrows(ClientException.class, () -> s.run("MATCH (n RETURN n").consume());
            assertEquals("Neo.ClientError.Statement.SyntaxError", e.code());
            assertEquals(1, s.run("RETURN 1 AS x").single().get("x").asInt());
        }
    }

    @Test
    void constraintViolation() {
        try (Session s = session()) {
            s.run("CREATE CONSTRAINT smoke_u IF NOT EXISTS FOR (u:U) REQUIRE u.id IS UNIQUE").consume();
            try {
                s.run("CREATE (:U {id: 1})").consume();
                ClientException e = assertThrows(ClientException.class,
                        () -> s.run("CREATE (:U {id: 1})").consume());
                assertEquals("Neo.ClientError.Schema.ConstraintValidationFailed", e.code());
                assertThrows(ClientException.class, () -> s.executeWriteWithoutResult(
                        tx -> tx.run("CREATE (:U {id: 1})").consume()));
                assertEquals(1, s.run("MATCH (u:U) RETURN count(u) AS c").single().get("c").asInt());
            } finally {
                s.run("DROP CONSTRAINT smoke_u").consume();
            }
        }
    }

    @Test
    void errorInsideTransactionRollsBack() {
        try (Session s = session()) {
            try (Transaction tx = s.beginTransaction()) {
                tx.run("CREATE (:E)").consume();
                assertThrows(ClientException.class, () -> tx.run("RETURN 1 / 0").consume());
            }
            assertEquals(0, s.run("MATCH (e:E) RETURN count(e) AS c").single().get("c").asInt());
        }
    }

    @Test
    void unknownDatabase() {
        try (Session s = driver.session(SessionConfig.forDatabase("no_such_db"))) {
            FatalDiscoveryException e = assertThrows(FatalDiscoveryException.class,
                    () -> s.run("RETURN 1").consume());
            assertEquals("Neo.ClientError.Database.DatabaseNotFound", e.code());
        }
    }

    // --- databases ------------------------------------------------------------

    @Test
    void otherDatabase() {
        QueryConfig system = QueryConfig.builder().withDatabase("system").build();
        driver.executableQuery("CREATE DATABASE smoke2j IF NOT EXISTS").withConfig(system).execute();
        try {
            List<String> names = driver.executableQuery("SHOW DATABASES").withConfig(system).execute()
                    .records().stream().map(r -> r.get("name").asString()).toList();
            assertTrue(names.contains("smoke2j"), names.toString());
            QueryConfig other = QueryConfig.builder().withDatabase("smoke2j").build();
            driver.executableQuery("CREATE (:OnlyHere)").withConfig(other).execute();
            assertEquals(1, driver.executableQuery("MATCH (n:OnlyHere) RETURN count(n) AS c")
                    .withConfig(other).execute().records().get(0).get("c").asInt());
            assertEquals(0, driver.executableQuery("MATCH (n:OnlyHere) RETURN count(n) AS c")
                    .execute().records().get(0).get("c").asInt());
        } finally {
            try (Session s = driver.session(SessionConfig.forDatabase("system"))) {
                s.run("DROP DATABASE smoke2j IF EXISTS").consume();
            }
        }
    }

    @Test
    void systemDatabaseRejectsQueries() {
        try (Session s = driver.session(SessionConfig.forDatabase("system"))) {
            assertThrows(ClientException.class, () -> s.run("MATCH (n) RETURN n").consume());
        }
    }

    // --- helpers --------------------------------------------------------------

    private static Set<String> toSet(Iterable<String> it) {
        Set<String> out = new java.util.HashSet<>();
        it.forEach(out::add);
        return Collections.unmodifiableSet(out);
    }

    private static final class LongRange {
        static List<Long> of(long from, long to) {
            return java.util.stream.LongStream.rangeClosed(from, to).boxed().toList();
        }
    }
}
