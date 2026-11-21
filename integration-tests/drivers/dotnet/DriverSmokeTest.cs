using Neo4j.Driver;
using Xunit;

namespace Graphbuff.Smoke;

/// <summary>Smoke suite: the official Neo4j .NET driver against graphbuff-server over Bolt.</summary>
/// <remarks>One class, so xUnit runs the tests one after another against one server.</remarks>
public sealed class DriverSmokeTest : IClassFixture<Server>, IAsyncLifetime
{
    private readonly Server _server;
    private readonly IDriver _driver;

    public DriverSmokeTest(Server server)
    {
        _server = server;
        _driver = GraphDatabase.Driver(server.BoltUri, AuthTokens.Basic("neo4j", Server.Token));
    }

    public async Task InitializeAsync()
    {
        await _driver.ExecutableQuery("MATCH (n) DETACH DELETE n").ExecuteAsync();
    }

    public async Task DisposeAsync()
    {
        await _driver.DisposeAsync();
    }

    // --- helpers ------------------------------------------------------------

    private async Task<T> WithSession<T>(Func<IAsyncSession, Task<T>> work, Action<SessionConfigBuilder>? config = null)
    {
        var session = config == null ? _driver.AsyncSession() : _driver.AsyncSession(config);
        try
        {
            return await work(session);
        }
        finally
        {
            await session.DisposeAsync();
        }
    }

    private Task WithSession(Func<IAsyncSession, Task> work, Action<SessionConfigBuilder>? config = null) =>
        WithSession<bool>(async s => { await work(s); return true; }, config);

    private static async Task<IRecord> Single(IAsyncQueryRunner runner, string query, object? parameters = null)
    {
        var cursor = parameters == null ? await runner.RunAsync(query) : await runner.RunAsync(query, parameters);
        return await cursor.SingleAsync();
    }

    private static async Task<List<long>> Longs(IAsyncQueryRunner runner, string query)
    {
        var cursor = await runner.RunAsync(query);
        var records = await cursor.ToListAsync();
        return records.Select(r => r[0].As<long>()).ToList();
    }

    private static async Task<IResultSummary> Consume(IAsyncQueryRunner runner, string query)
    {
        var cursor = await runner.RunAsync(query);
        return await cursor.ConsumeAsync();
    }

    private static async Task<Neo4jException> Fails(Func<Task> work)
    {
        try
        {
            await work();
        }
        catch (Neo4jException e)
        {
            return e;
        }
        throw new Xunit.Sdk.XunitException("expected a Neo4jException");
    }

    private static (int Major, int Minor) Protocol(IServerInfo info)
    {
        var parts = info.ProtocolVersion.Split('.');
        return (int.Parse(parts[0]), int.Parse(parts[1]));
    }

    // --- connection ---------------------------------------------------------

    [Fact]
    public async Task VerifyConnectivityAndServerInfo()
    {
        await _driver.VerifyConnectivityAsync();
        var info = await _driver.GetServerInfoAsync();
        Assert.StartsWith("Neo4j/", info.Agent);
        var (major, minor) = Protocol(info);
        Assert.True(major > 4 || (major == 4 && minor >= 4));
    }

    [Fact]
    public async Task NewestBoltAndGqlStatuses()
    {
        // The newest Bolt both speak: 5.8 for drivers that know it (5.28 and 6.x).
        Assert.Equal((5, 8), Protocol(await _driver.GetServerInfoAsync()));
        await WithSession(async s =>
        {
            var summary = await Consume(s, "UNWIND [1, 2] AS x RETURN x");
            Assert.Equal("00000", summary.GqlStatusObjects.First().GqlStatus);
            summary = await Consume(s, "UNWIND [] AS x RETURN x");
            Assert.Equal("02000", summary.GqlStatusObjects.First().GqlStatus);
            var e = await Fails(() => Consume(s, "RETURN 1 +"));
            Assert.Equal("Neo.ClientError.Statement.SyntaxError", e.Code);
            // Neo4jException.GqlStatus exists from driver 6.0.
            var gqlStatus = typeof(Neo4jException).GetProperty("GqlStatus");
            if (gqlStatus != null) Assert.Equal("42001", gqlStatus.GetValue(e));
        });
    }

    [Fact]
    public async Task RoutingScheme()
    {
        await using var d = GraphDatabase.Driver(_server.Neo4jUri, AuthTokens.Basic("neo4j", Server.Token));
        await d.VerifyConnectivityAsync();
        var read = await d.ExecutableQuery("RETURN 1 AS x")
            .WithConfig(new QueryConfig(RoutingControl.Readers)).ExecuteAsync();
        Assert.Equal(1L, read.Result[0]["x"].As<long>());
        var write = await d.ExecutableQuery("RETURN 2 AS x")
            .WithConfig(new QueryConfig(RoutingControl.Writers)).ExecuteAsync();
        Assert.Equal(2L, write.Result[0]["x"].As<long>());
    }

    [Fact]
    public async Task WrongPassword()
    {
        await using var d = GraphDatabase.Driver(_server.BoltUri, AuthTokens.Basic("neo4j", "wrong"));
        await Assert.ThrowsAsync<AuthenticationException>(() => d.VerifyConnectivityAsync());
    }

    [Fact]
    public async Task BearerAuth()
    {
        await using var d = GraphDatabase.Driver(_server.BoltUri, AuthTokens.Bearer(Server.Token));
        await d.VerifyConnectivityAsync();
    }

    // --- values -------------------------------------------------------------

    [Fact]
    public Task ScalarValues() => WithSession(async s =>
    {
        var rec = await Single(s,
            "RETURN 1 AS i, -9007199254740993 AS big, 9223372036854775807 AS max, 1.5 AS f," +
            " 'héllo' AS s, true AS b, null AS n, [1, 'a', null] AS l, {k: [1, {x: 2}]} AS m");
        Assert.IsType<long>(rec["i"]);
        Assert.Equal(1L, rec["i"]);
        Assert.Equal(-9007199254740993L, rec["big"]);
        Assert.Equal(long.MaxValue, rec["max"]);
        Assert.Equal(1.5, rec["f"]);
        Assert.Equal("héllo", rec["s"]);
        Assert.Equal(true, rec["b"]);
        Assert.Null(rec["n"]);
        Assert.Equal(new object?[] { 1L, "a", null }, rec["l"].As<List<object?>>());
        var m = rec["m"].As<IDictionary<string, object>>();
        var inner = m["k"].As<List<object>>()[1].As<IDictionary<string, object>>();
        Assert.Equal(2L, inner["x"]);
        Assert.Equal(new[] { "i", "big", "max", "f", "s", "b", "n", "l", "m" }, rec.Keys);
    });

    [Fact]
    public Task ParametersRoundTrip() => WithSession(async s =>
    {
        var text = new string('x', 70000);
        var list = Enumerable.Range(0, 300).Select(k => (long)k).ToList();
        var parameters = new Dictionary<string, object?>
        {
            ["i"] = 1L << 62, ["int32"] = 7, ["f"] = -0.25, ["s"] = text, ["b"] = false, ["n"] = null,
            ["l"] = list,
            ["m"] = new Dictionary<string, object> { ["nested"] = new Dictionary<string, object> { ["deep"] = new List<object> { "a", 1L, 2.5 } } },
            ["el"] = new List<object>(), ["em"] = new Dictionary<string, object>()
        };
        var rec = await Single(s,
            "RETURN $i AS i, $int32 AS int32, valueType($int32) AS int32Type, $f AS f, $s AS s, $b AS b," +
            " $n AS n, $l AS l, $m AS m, $el AS el, $em AS em", parameters);
        Assert.Equal(1L << 62, rec["i"]);
        // A C# int is sent as a Cypher integer.
        Assert.Equal(7L, rec["int32"]);
        Assert.StartsWith("INTEGER", rec["int32Type"].As<string>());
        Assert.Equal(-0.25, rec["f"]);
        Assert.Equal(text, rec["s"]);
        Assert.Equal(false, rec["b"]);
        Assert.Null(rec["n"]);
        Assert.Equal(list, rec["l"].As<List<long>>());
        var deep = rec["m"].As<IDictionary<string, object>>()["nested"].As<IDictionary<string, object>>()["deep"].As<List<object>>();
        Assert.Equal(new object[] { "a", 1L, 2.5 }, deep);
        Assert.Empty(rec["el"].As<List<object>>());
        Assert.Empty(rec["em"].As<IDictionary<string, object>>());
    });

    [Fact]
    public Task TemporalValues() => WithSession(async s =>
    {
        var rec = await Single(s,
            "RETURN date('2024-02-29') AS d, localtime('12:34:56.789') AS lt," +
            " time('12:00:00+02:00') AS t, localdatetime('2024-01-02T03:04:05') AS ldt," +
            " datetime('2024-01-02T03:04:05.123456789+01:00') AS dt," +
            " datetime('2024-01-02T03:04:05[Europe/Prague]') AS zdt," +
            " duration('P1Y2M3DT4H5M6.5S') AS dur");
        var d = rec["d"].As<LocalDate>();
        Assert.Equal((2024, 2, 29), (d.Year, d.Month, d.Day));
        var lt = rec["lt"].As<LocalTime>();
        Assert.Equal((12, 34, 56, 789000000), (lt.Hour, lt.Minute, lt.Second, lt.Nanosecond));
        var t = rec["t"].As<OffsetTime>();
        Assert.Equal((12, 7200), (t.Hour, t.OffsetSeconds));
        var ldt = rec["ldt"].As<LocalDateTime>();
        Assert.Equal((2024, 1, 2, 3, 4, 5), (ldt.Year, ldt.Month, ldt.Day, ldt.Hour, ldt.Minute, ldt.Second));
        var dt = rec["dt"].As<ZonedDateTime>();
        Assert.Equal((2024, 1, 2, 3, 4, 5), (dt.Year, dt.Month, dt.Day, dt.Hour, dt.Minute, dt.Second));
        Assert.Equal(123456789, dt.Nanosecond);
        Assert.Equal(3600, dt.OffsetSeconds);
        var zdt = rec["zdt"].As<ZonedDateTime>();
        Assert.Equal("Europe/Prague", Assert.IsType<ZoneId>(zdt.Zone).Id);
        Assert.Equal(3, zdt.Hour);
        var dur = rec["dur"].As<Duration>();
        Assert.Equal(14L, dur.Months);
        Assert.Equal(3L, dur.Days);
        Assert.Equal(4L * 3600 + 5 * 60 + 6, dur.Seconds);
        Assert.Equal(500000000, dur.Nanos);
    });

    [Fact]
    public Task TemporalParameters() => WithSession(async s =>
    {
        var parameters = new Dictionary<string, object>
        {
            ["d"] = new LocalDate(2001, 2, 3),
            ["ldt"] = new LocalDateTime(2001, 2, 3, 4, 5, 6),
            ["dt"] = new ZonedDateTime(new DateTimeOffset(2001, 2, 3, 4, 5, 6, TimeSpan.FromHours(-5))),
            ["zdt"] = new ZonedDateTime(new DateTime(2001, 2, 3, 4, 5, 6), "Europe/Prague"),
            ["native"] = new DateTimeOffset(2001, 2, 3, 4, 5, 6, TimeSpan.Zero),
            ["lt"] = new LocalTime(7, 8, 9),
            ["dur"] = new Duration(0L, 2L, 30L, 0)
        };
        var rec = await Single(s,
            "RETURN toString($d) AS d, toString($ldt) AS ldt, toString($dt) AS dt," +
            " toString($zdt) AS zdt, $native.epochSeconds AS epoch, toString($lt) AS lt," +
            " toString($dur) AS dur", parameters);
        Assert.Equal("2001-02-03", rec["d"]);
        Assert.Equal("2001-02-03T04:05:06", rec["ldt"]);
        Assert.Equal("2001-02-03T04:05:06-05:00", rec["dt"]);
        Assert.Equal("2001-02-03T04:05:06+01:00[Europe/Prague]", rec["zdt"]);
        Assert.Equal(new DateTimeOffset(2001, 2, 3, 4, 5, 6, TimeSpan.Zero).ToUnixTimeSeconds(), rec["epoch"]);
        Assert.Equal("07:08:09", rec["lt"]);
        Assert.Equal("P2DT30S", rec["dur"]);
        // Round trip unchanged.
        var back = await Single(s, "RETURN $dt AS dt, $d AS d", parameters);
        Assert.Equal(parameters["dt"], back["dt"].As<ZonedDateTime>());
        Assert.Equal(parameters["d"], back["d"].As<LocalDate>());
    });

    [Fact]
    public Task SpatialValues() => WithSession(async s =>
    {
        var rec = await Single(s,
            "RETURN point({x: 1.5, y: -2}) AS c, point({x: 1, y: 2, z: 3}) AS c3," +
            " point({longitude: 14.42, latitude: 50.09}) AS g");
        var c = rec["c"].As<Point>();
        Assert.Equal((7203, 1.5, -2.0), (c.SrId, c.X, c.Y));
        var c3 = rec["c3"].As<Point>();
        Assert.Equal((9157, 1.0, 2.0, 3.0), (c3.SrId, c3.X, c3.Y, c3.Z));
        var g = rec["g"].As<Point>();
        Assert.Equal((4326, 14.42, 50.09), (g.SrId, g.X, g.Y));
        var r = await Single(s, "RETURN $p.x AS x, point.distance($p, point({x: 0, y: 0})) AS d",
            new { p = new Point(7203, 3, 4) });
        Assert.Equal(3.0, r["x"]);
        Assert.Equal(5.0, r["d"]);
    });

    [Fact]
    public Task GraphValues() => WithSession(async s =>
    {
        var rec = await Single(s,
            "CREATE p = (a:Person:Admin {name: 'Ann', age: 30})-[r:KNOWS {since: 2020}]->" +
            "(b:Person {name: 'Bob'}) RETURN a, r, b, p");
        var a = rec["a"].As<INode>();
        var r = rec["r"].As<IRelationship>();
        var b = rec["b"].As<INode>();
        var p = rec["p"].As<IPath>();
        Assert.Equal(new[] { "Admin", "Person" }, a.Labels.OrderBy(l => l));
        Assert.Equal("Ann", a["name"]);
        Assert.Equal(30L, a["age"]);
        Assert.Equal("KNOWS", r.Type);
        Assert.Equal(2020L, r["since"]);
        Assert.Equal(a.ElementId, r.StartNodeElementId);
        Assert.Equal(b.ElementId, r.EndNodeElementId);
        Assert.Single(p.Relationships);
        Assert.Equal(new object[] { "Ann", "Bob" }, p.Nodes.Select(n => n["name"]));
        // Element ids are stable and usable from Cypher.
        var found = await Single(s, "MATCH (n) WHERE elementId(n) = $id RETURN n.name AS name", new { id = b.ElementId });
        Assert.Equal("Bob", found["name"]);
    });

    [Fact]
    public Task ReverseRelationshipInPath() => WithSession(async s =>
    {
        await Consume(s, "CREATE (:A {n: 1})-[:R]->(:B {n: 2})");
        var p = (await Single(s, "MATCH p = (:B)<-[:R]-(:A) RETURN p"))["p"].As<IPath>();
        Assert.Equal(new object[] { 2L, 1L }, p.Nodes.Select(n => n["n"]));
        var rel = p.Relationships[0];
        Assert.Equal(p.End.ElementId, rel.StartNodeElementId);
        Assert.Equal(p.Start.ElementId, rel.EndNodeElementId);
    });

    // --- results ------------------------------------------------------------

    [Fact]
    public Task StreamingWithSmallFetchSize() => WithSession(async s =>
    {
        var cursor = await s.RunAsync("UNWIND range(1, 1000) AS x RETURN x");
        var seen = new List<long>();
        while (await cursor.FetchAsync()) seen.Add(cursor.Current["x"].As<long>());
        Assert.Equal(Enumerable.Range(1, 1000).Select(k => (long)k), seen);
    }, o => o.WithFetchSize(7));

    [Fact]
    public Task PartlyReadResults() => WithSession(async s =>
    {
        var first = await s.RunAsync("UNWIND range(1, 100) AS x RETURN x");
        Assert.True(await first.FetchAsync());
        Assert.Equal(1L, first.Current["x"]);
        // A new query in the same session discards the rest of the first.
        Assert.Equal(42L, (await Single(s, "RETURN 42 AS x"))["x"]);
    }, o => o.WithFetchSize(10));

    [Fact]
    public Task SummaryCounters() => WithSession(async s =>
    {
        var summary = await Consume(s, "CREATE (a:L {p: 1})-[:T]->(b:L), (c:M) SET c.q = 2 RETURN a");
        var c = summary.Counters;
        Assert.Equal(3, c.NodesCreated);
        Assert.Equal(1, c.RelationshipsCreated);
        Assert.Equal(3, c.LabelsAdded);
        Assert.Equal(2, c.PropertiesSet);
        Assert.True(c.ContainsUpdates);
        Assert.Equal(QueryType.ReadWrite, summary.QueryType);
        summary = await Consume(s, "MATCH (n) DETACH DELETE n");
        Assert.Equal(3, summary.Counters.NodesDeleted);
        Assert.Equal(1, summary.Counters.RelationshipsDeleted);
        Assert.Equal(QueryType.ReadOnly, (await Consume(s, "RETURN 1")).QueryType);
    });

    [Fact]
    public Task SchemaCounters() => WithSession(async s =>
    {
        var summary = await Consume(s, "CREATE INDEX smoke_idx IF NOT EXISTS FOR (n:Smoke) ON (n.k)");
        Assert.Equal(1, summary.Counters.IndexesAdded);
        Assert.Equal(QueryType.SchemaWrite, summary.QueryType);
        summary = await Consume(s, "DROP INDEX smoke_idx");
        Assert.Equal(1, summary.Counters.IndexesRemoved);
    });

    [Fact]
    public Task ExplainAndProfile() => WithSession(async s =>
    {
        var summary = await Consume(s, "EXPLAIN MATCH (n) RETURN n");
        Assert.True(summary.HasPlan);
        Assert.NotNull(summary.Plan);
        await Consume(s, "CREATE (:X), (:X)");
        summary = await Consume(s, "PROFILE MATCH (n:X) RETURN n");
        Assert.True(summary.HasProfile);
        Assert.Equal(2L, summary.Profile.Records);
    });

    [Fact]
    public async Task ExecutableQueryApi()
    {
        await _driver.ExecutableQuery("CREATE (:Q {v: $v})").WithParameters(new { v = 7 }).ExecuteAsync();
        var result = await _driver.ExecutableQuery("MATCH (q:Q) RETURN q.v AS v").ExecuteAsync();
        Assert.Equal(new[] { "v" }, result.Keys);
        Assert.Equal(new[] { 7L }, result.Result.Select(r => r["v"].As<long>()));
        Assert.False(string.IsNullOrEmpty(result.Summary.Database.Name));
        var mapped = await _driver.ExecutableQuery("UNWIND [1, 2, 3] AS x RETURN x")
            .WithMap(r => r["x"].As<long>() * 10).ExecuteAsync();
        Assert.Equal(new[] { 10L, 20L, 30L }, mapped.Result);
    }

    [Fact]
    public Task RecordHelpers() => WithSession(async s =>
    {
        var rec = await Single(s, "RETURN 1 AS a, 2 AS b");
        Assert.Equal(1L, rec[0]);
        Assert.Equal(2L, rec.Get<long>("b"));
        Assert.Equal(1L, rec.Values["a"]);
        Assert.True(rec.TryGet<long>("a", out var a) && a == 1);
        Assert.Equal(new[] { 1L, 2L, 3L }, await Longs(s, "UNWIND [1, 2, 3] AS x RETURN x"));
        var none = await (await s.RunAsync("RETURN 1 AS x LIMIT 0")).ToListAsync();
        Assert.Empty(none);
    });

    // --- transactions -------------------------------------------------------

    [Fact]
    public Task ManagedTransactions() => WithSession(async s =>
    {
        foreach (var name in new[] { "a", "b" })
            await s.ExecuteWriteAsync(tx => tx.RunAsync("CREATE (:Acc {name: $n, bal: 100})", new { n = name }));
        var total = await s.ExecuteReadAsync(async tx => (await Single(tx, "MATCH (a:Acc) RETURN sum(a.bal) AS s"))["s"].As<long>());
        Assert.Equal(200L, total);
    });

    [Fact]
    public Task ExplicitCommitAndRollback() => WithSession(async s =>
    {
        var tx = await s.BeginTransactionAsync();
        await Consume(tx, "CREATE (:T {v: 1})");
        // Visible inside the transaction...
        Assert.Equal(1L, (await Single(tx, "MATCH (t:T) RETURN count(t) AS c"))["c"]);
        await tx.CommitAsync();
        tx = await s.BeginTransactionAsync();
        await Consume(tx, "CREATE (:T {v: 2})");
        await tx.RollbackAsync();
        await using (var open = await s.BeginTransactionAsync())
        {
            await Consume(open, "CREATE (:T {v: 3})");
            // disposed without commit: rolled back
        }
        Assert.Equal(new[] { 1L }, await Longs(s, "MATCH (t:T) RETURN t.v AS v ORDER BY v"));
    });

    [Fact]
    public Task SeveralOpenResultsInOneTransaction() => WithSession(async s =>
    {
        var tx = await s.BeginTransactionAsync();
        var r1 = await tx.RunAsync("UNWIND range(1, 5) AS x RETURN x");
        var r2 = await tx.RunAsync("UNWIND range(10, 15) AS y RETURN y");
        Assert.Equal(new[] { 10L, 11L, 12L, 13L, 14L, 15L }, (await r2.ToListAsync()).Select(r => r["y"].As<long>()));
        Assert.Equal(new[] { 1L, 2L, 3L, 4L, 5L }, (await r1.ToListAsync()).Select(r => r["x"].As<long>()));
        await tx.CommitAsync();
    }, o => o.WithFetchSize(2));

    [Fact]
    public Task TransactionMetadataAndTimeoutConfig() => WithSession(async s =>
    {
        var cursor = await s.RunAsync("CREATE (:Meta) RETURN 1 AS x", new Dictionary<string, object>(),
            c => c.WithMetadata(new Dictionary<string, object> { ["app"] = "smoke" }).WithTimeout(TimeSpan.FromSeconds(10)));
        Assert.Equal(1L, (await cursor.SingleAsync())["x"]);
        var tx = await s.BeginTransactionAsync(c =>
            c.WithMetadata(new Dictionary<string, object> { ["k"] = "v" }).WithTimeout(TimeSpan.FromSeconds(10)));
        await Consume(tx, "RETURN 1");
        await tx.CommitAsync();
    });

    [Fact]
    public Task QueryTimeout() => WithSession(async s =>
    {
        var e = await Fails(async () =>
        {
            var cursor = await s.RunAsync(
                "UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*)",
                new Dictionary<string, object>(), c => c.WithTimeout(TimeSpan.FromMilliseconds(200)));
            await cursor.ConsumeAsync();
        });
        Assert.Contains("Transaction", e.Code);
        // The session recovers.
        Assert.Equal(1L, (await Single(s, "RETURN 1 AS x"))["x"]);
    });

    [Fact]
    public async Task BookmarksChainSessions()
    {
        var bookmarks = await WithSession(async s =>
        {
            await Consume(s, "CREATE (:Bm {v: 1})");
            return s.LastBookmarks;
        });
        await WithSession(async s =>
        {
            Assert.True((await Single(s, "MATCH (b:Bm) RETURN count(b) AS c"))["c"].As<long>() >= 1);
        }, o => o.WithBookmarks(bookmarks));
    }

    [Fact]
    public async Task BookmarkManager()
    {
        var manager = GraphDatabase.BookmarkManagerFactory.NewBookmarkManager();
        await WithSession(s => Consume(s, "CREATE (:Bm2)"), o => o.WithBookmarkManager(manager));
        await WithSession(async s =>
        {
            Assert.Equal(1L, (await Single(s, "MATCH (b:Bm2) RETURN count(b) AS c"))["c"]);
        }, o => o.WithBookmarkManager(manager));
    }

    [Fact]
    public async Task ConcurrentSessions()
    {
        await Task.WhenAll(Enumerable.Range(0, 8).Select(i => WithSession(async s =>
        {
            for (var j = 0; j < 20; j++)
            {
                var jj = j;
                await s.ExecuteWriteAsync(tx => tx.RunAsync("CREATE (:C {w: $i, j: $j})", new { i, j = jj }));
            }
        })));
        var result = await _driver.ExecutableQuery("MATCH (c:C) RETURN count(c) AS n").ExecuteAsync();
        Assert.Equal(160L, result.Result[0]["n"]);
    }

    // --- errors -------------------------------------------------------------

    [Fact]
    public Task SyntaxError() => WithSession(async s =>
    {
        var e = await Fails(() => Consume(s, "MATCH (n RETURN n"));
        Assert.IsAssignableFrom<ClientException>(e);
        Assert.Equal("Neo.ClientError.Statement.SyntaxError", e.Code);
        Assert.Equal(1L, (await Single(s, "RETURN 1 AS x"))["x"]);
    });

    [Fact]
    public Task ConstraintViolation() => WithSession(async s =>
    {
        await Consume(s, "CREATE CONSTRAINT smoke_u IF NOT EXISTS FOR (u:U) REQUIRE u.id IS UNIQUE");
        try
        {
            await Consume(s, "CREATE (:U {id: 1})");
            var e = await Fails(() => Consume(s, "CREATE (:U {id: 1})"));
            Assert.Equal("Neo.ClientError.Schema.ConstraintValidationFailed", e.Code);
            e = await Fails(() => s.ExecuteWriteAsync(tx => tx.RunAsync("CREATE (:U {id: 1})")));
            Assert.Equal("Neo.ClientError.Schema.ConstraintValidationFailed", e.Code);
            Assert.Equal(1L, (await Single(s, "MATCH (u:U) RETURN count(u) AS c"))["c"]);
        }
        finally
        {
            await Consume(s, "DROP CONSTRAINT smoke_u");
        }
    });

    [Fact]
    public Task ErrorInsideTransactionRollsBack() => WithSession(async s =>
    {
        await using (var tx = await s.BeginTransactionAsync())
        {
            await Consume(tx, "CREATE (:E)");
            var e = await Fails(() => Consume(tx, "RETURN 1 / 0"));
            Assert.StartsWith("Neo.ClientError.", e.Code);
        }
        Assert.Equal(0L, (await Single(s, "MATCH (e:E) RETURN count(e) AS c"))["c"]);
    });

    [Fact]
    public Task UnknownDatabase() => WithSession(async s =>
    {
        var e = await Fails(() => Consume(s, "RETURN 1"));
        Assert.Equal("Neo.ClientError.Database.DatabaseNotFound", e.Code);
    }, o => o.WithDatabase("no_such_db"));

    // --- databases ----------------------------------------------------------

    private static QueryConfig On(string database) => new(database: database);

    [Fact]
    public async Task OtherDatabase()
    {
        // Administration goes through the `system` database, as with Neo4j.
        await _driver.ExecutableQuery("CREATE DATABASE smoke2 IF NOT EXISTS").WithConfig(On("system")).ExecuteAsync();
        try
        {
            var dbs = await _driver.ExecutableQuery("SHOW DATABASES").WithConfig(On("system")).ExecuteAsync();
            Assert.Contains("smoke2", dbs.Result.Select(r => r["name"].As<string>()));
            await _driver.ExecutableQuery("CREATE (:OnlyHere)").WithConfig(On("smoke2")).ExecuteAsync();
            var here = await _driver.ExecutableQuery("MATCH (n:OnlyHere) RETURN count(n) AS c").WithConfig(On("smoke2")).ExecuteAsync();
            Assert.Equal(1L, here.Result[0]["c"]);
            var home = await _driver.ExecutableQuery("MATCH (n:OnlyHere) RETURN count(n) AS c").ExecuteAsync();
            Assert.Equal(0L, home.Result[0]["c"]);
            await WithSession(async s =>
            {
                Assert.Equal(1L, (await Single(s, "MATCH (n) RETURN count(n) AS c"))["c"]);
            }, o => o.WithDatabase("smoke2"));
        }
        finally
        {
            await _driver.ExecutableQuery("DROP DATABASE smoke2 IF EXISTS").WithConfig(On("system")).ExecuteAsync();
        }
    }

    [Fact]
    public Task SystemDatabaseRejectsQueries() => WithSession(async s =>
    {
        var e = await Fails(() => Consume(s, "MATCH (n) RETURN n"));
        Assert.StartsWith("Neo.ClientError.", e.Code);
    }, o => o.WithDatabase("system"));
}
