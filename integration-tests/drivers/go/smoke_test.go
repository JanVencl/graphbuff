// Smoke suite: the official Neo4j Go driver against graphbuff-server over Bolt.
//
// Written against the v5 API. The v6 driver keeps the v5 names (`*WithContext`) as
// aliases, so the suite also runs on v6 once the import path says /v6 (see README).
package smoke

import (
	"context"
	"errors"
	"fmt"
	"os"
	"reflect"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/neo4j/neo4j-go-driver/v5/neo4j"
	"github.com/neo4j/neo4j-go-driver/v5/neo4j/db"
)

var (
	srv    *server
	driver neo4j.DriverWithContext
	ctx    = context.Background()
)

func TestMain(m *testing.M) {
	var err error
	srv, err = startServer()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	driver, err = neo4j.NewDriverWithContext(srv.boltURI(), neo4j.BasicAuth("neo4j", token, ""))
	if err != nil {
		srv.stop()
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	code := m.Run()
	_ = driver.Close(ctx)
	srv.stop()
	os.Exit(code)
}

// --- helpers ----------------------------------------------------------------

// clean empties the default database and returns a session that is closed after the test.
func clean(t *testing.T, config ...neo4j.SessionConfig) neo4j.SessionWithContext {
	t.Helper()
	if _, err := neo4j.ExecuteQuery(ctx, driver, "MATCH (n) DETACH DELETE n", nil, neo4j.EagerResultTransformer); err != nil {
		t.Fatal(err)
	}
	return session(t, config...)
}

func session(t *testing.T, config ...neo4j.SessionConfig) neo4j.SessionWithContext {
	t.Helper()
	c := neo4j.SessionConfig{}
	if len(config) > 0 {
		c = config[0]
	}
	s := driver.NewSession(ctx, c)
	t.Cleanup(func() { _ = s.Close(ctx) })
	return s
}

type runner interface {
	Run(ctx context.Context, cypher string, params map[string]any) (neo4j.ResultWithContext, error)
}

// sessionRunner adapts a session (whose Run takes transaction configurers) to runner.
type sessionRunner struct{ s neo4j.SessionWithContext }

func (r sessionRunner) Run(ctx context.Context, cypher string, params map[string]any) (neo4j.ResultWithContext, error) {
	return r.s.Run(ctx, cypher, params)
}

func on(x any) runner {
	switch v := x.(type) {
	case neo4j.SessionWithContext:
		return sessionRunner{v}
	case runner:
		return v
	}
	panic(fmt.Sprintf("not a query runner: %T", x))
}

func single(t *testing.T, x any, cypher string, params map[string]any) *neo4j.Record {
	t.Helper()
	res, err := on(x).Run(ctx, cypher, params)
	if err != nil {
		t.Fatal(err)
	}
	rec, err := res.Single(ctx)
	if err != nil {
		t.Fatal(err)
	}
	return rec
}

func consume(x any, cypher string, params map[string]any) (neo4j.ResultSummary, error) {
	res, err := on(x).Run(ctx, cypher, params)
	if err != nil {
		return nil, err
	}
	return res.Consume(ctx)
}

func mustConsume(t *testing.T, x any, cypher string, params map[string]any) neo4j.ResultSummary {
	t.Helper()
	s, err := consume(x, cypher, params)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func get(t *testing.T, rec *neo4j.Record, key string) any {
	t.Helper()
	v, ok := rec.Get(key)
	if !ok {
		t.Fatalf("no %q in record %v", key, rec.Keys)
	}
	return v
}

func ints(t *testing.T, x any, cypher string) []int64 {
	t.Helper()
	res, err := on(x).Run(ctx, cypher, nil)
	if err != nil {
		t.Fatal(err)
	}
	recs, err := res.Collect(ctx)
	if err != nil {
		t.Fatal(err)
	}
	out := []int64{}
	for _, r := range recs {
		out = append(out, r.Values[0].(int64))
	}
	return out
}

func neoError(t *testing.T, err error) *db.Neo4jError {
	t.Helper()
	if err == nil {
		t.Fatal("expected an error")
	}
	var e *db.Neo4jError
	if !errors.As(err, &e) {
		t.Fatalf("expected a Neo4jError, got %T: %v", err, err)
	}
	return e
}

func eq(t *testing.T, want, got any) {
	t.Helper()
	if !reflect.DeepEqual(want, got) {
		t.Fatalf("want %#v, got %#v", want, got)
	}
}

func rangeInts(from, to int64) []int64 {
	out := []int64{}
	for i := from; i <= to; i++ {
		out = append(out, i)
	}
	return out
}

// --- connection -------------------------------------------------------------

func TestVerifyConnectivityAndServerInfo(t *testing.T) {
	if err := driver.VerifyConnectivity(ctx); err != nil {
		t.Fatal(err)
	}
	info, err := driver.GetServerInfo(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(info.Agent(), "Neo4j/") {
		t.Fatalf("agent %q", info.Agent())
	}
	v := info.ProtocolVersion()
	if v.Major < 4 || (v.Major == 4 && v.Minor < 4) {
		t.Fatalf("protocol %v", v)
	}
}

func TestNewestBoltAndGqlStatuses(t *testing.T) {
	// The newest Bolt both speak: 5.8 for drivers that know it (5.28 and 6.x).
	info, err := driver.GetServerInfo(ctx)
	if err != nil {
		t.Fatal(err)
	}
	eq(t, db.ProtocolVersion{Major: 5, Minor: 8}, info.ProtocolVersion())
	s := clean(t)
	summary := mustConsume(t, s, "UNWIND [1, 2] AS x RETURN x", nil)
	eq(t, "00000", summary.GqlStatusObjects()[0].GqlStatus())
	summary = mustConsume(t, s, "UNWIND [] AS x RETURN x", nil)
	eq(t, "02000", summary.GqlStatusObjects()[0].GqlStatus())
	_, err = consume(s, "RETURN 1 +", nil)
	e := neoError(t, err)
	eq(t, "Neo.ClientError.Statement.SyntaxError", e.Code)
	eq(t, "42001", e.GqlStatus)
}

func TestRoutingScheme(t *testing.T) {
	d, err := neo4j.NewDriverWithContext(srv.neo4jURI(), neo4j.BasicAuth("neo4j", token, ""))
	if err != nil {
		t.Fatal(err)
	}
	defer d.Close(ctx)
	if err := d.VerifyConnectivity(ctx); err != nil {
		t.Fatal(err)
	}
	r, err := neo4j.ExecuteQuery(ctx, d, "RETURN 1 AS x", nil, neo4j.EagerResultTransformer,
		neo4j.ExecuteQueryWithReadersRouting())
	if err != nil {
		t.Fatal(err)
	}
	eq(t, int64(1), r.Records[0].Values[0])
	r, err = neo4j.ExecuteQuery(ctx, d, "RETURN 2 AS x", nil, neo4j.EagerResultTransformer,
		neo4j.ExecuteQueryWithWritersRouting())
	if err != nil {
		t.Fatal(err)
	}
	eq(t, int64(2), r.Records[0].Values[0])
}

func TestWrongPassword(t *testing.T) {
	d, err := neo4j.NewDriverWithContext(srv.boltURI(), neo4j.BasicAuth("neo4j", "wrong", ""))
	if err != nil {
		t.Fatal(err)
	}
	defer d.Close(ctx)
	e := neoError(t, d.VerifyConnectivity(ctx))
	eq(t, "Neo.ClientError.Security.Unauthorized", e.Code)
}

func TestBearerAuth(t *testing.T) {
	d, err := neo4j.NewDriverWithContext(srv.boltURI(), neo4j.BearerAuth(token))
	if err != nil {
		t.Fatal(err)
	}
	defer d.Close(ctx)
	if err := d.VerifyConnectivity(ctx); err != nil {
		t.Fatal(err)
	}
}

// --- values -----------------------------------------------------------------

func TestScalarValues(t *testing.T) {
	s := clean(t)
	rec := single(t, s,
		"RETURN 1 AS i, -9007199254740993 AS big, 9223372036854775807 AS max, 1.5 AS f,"+
			" 'héllo' AS s, true AS b, null AS n, [1, 'a', null] AS l, {k: [1, {x: 2}]} AS m", nil)
	eq(t, int64(1), get(t, rec, "i"))
	eq(t, int64(-9007199254740993), get(t, rec, "big"))
	eq(t, int64(9223372036854775807), get(t, rec, "max"))
	eq(t, 1.5, get(t, rec, "f"))
	eq(t, "héllo", get(t, rec, "s"))
	eq(t, true, get(t, rec, "b"))
	eq(t, nil, get(t, rec, "n"))
	eq(t, []any{int64(1), "a", nil}, get(t, rec, "l"))
	eq(t, map[string]any{"k": []any{int64(1), map[string]any{"x": int64(2)}}}, get(t, rec, "m"))
	eq(t, []string{"i", "big", "max", "f", "s", "b", "n", "l", "m"}, rec.Keys)
}

func TestParametersRoundTrip(t *testing.T) {
	s := clean(t)
	list := []any{}
	for i := 0; i < 300; i++ {
		list = append(list, int64(i))
	}
	text := strings.Repeat("x", 70000)
	params := map[string]any{
		"i": int64(1) << 62, "goint": 7, "f": -0.25, "s": text, "b": false, "n": nil,
		"l": list, "m": map[string]any{"nested": map[string]any{"deep": []any{"a", int64(1), 2.5}}},
		"el": []any{}, "em": map[string]any{},
	}
	rec := single(t, s,
		"RETURN $i AS i, $goint AS goint, valueType($goint) AS gointType, $f AS f, $s AS s, $b AS b,"+
			" $n AS n, $l AS l, $m AS m, $el AS el, $em AS em", params)
	eq(t, int64(1)<<62, get(t, rec, "i"))
	// A Go int is sent as a Cypher integer.
	eq(t, int64(7), get(t, rec, "goint"))
	if !strings.HasPrefix(get(t, rec, "gointType").(string), "INTEGER") {
		t.Fatalf("goint type %v", get(t, rec, "gointType"))
	}
	eq(t, -0.25, get(t, rec, "f"))
	eq(t, text, get(t, rec, "s"))
	eq(t, false, get(t, rec, "b"))
	eq(t, nil, get(t, rec, "n"))
	eq(t, list, get(t, rec, "l"))
	eq(t, params["m"], get(t, rec, "m"))
	eq(t, []any{}, get(t, rec, "el"))
	eq(t, map[string]any{}, get(t, rec, "em"))
}

func TestTemporalValues(t *testing.T) {
	s := clean(t)
	rec := single(t, s,
		"RETURN date('2024-02-29') AS d, localtime('12:34:56.789') AS lt,"+
			" time('12:00:00+02:00') AS t, localdatetime('2024-01-02T03:04:05') AS ldt,"+
			" datetime('2024-01-02T03:04:05.123456789+01:00') AS dt,"+
			" datetime('2024-01-02T03:04:05[Europe/Prague]') AS zdt,"+
			" duration('P1Y2M3DT4H5M6.5S') AS dur", nil)
	d := time.Time(get(t, rec, "d").(neo4j.Date))
	eq(t, "2024-02-29", d.Format("2006-01-02"))
	lt := time.Time(get(t, rec, "lt").(neo4j.LocalTime))
	eq(t, "12:34:56.789", lt.Format("15:04:05.000"))
	tm := time.Time(get(t, rec, "t").(neo4j.Time))
	_, offset := tm.Zone()
	eq(t, []int{12, 7200}, []int{tm.Hour(), offset})
	ldt := time.Time(get(t, rec, "ldt").(neo4j.LocalDateTime))
	eq(t, "2024-01-02T03:04:05", ldt.Format("2006-01-02T15:04:05"))
	dt := get(t, rec, "dt").(time.Time)
	eq(t, "2024-01-02T03:04:05.123456789+01:00", dt.Format(time.RFC3339Nano))
	zdt := get(t, rec, "zdt").(time.Time)
	eq(t, "Europe/Prague", zdt.Location().String())
	eq(t, 3, zdt.Hour())
	eq(t, neo4j.Duration{Months: 14, Days: 3, Seconds: 4*3600 + 5*60 + 6, Nanos: 500000000}, get(t, rec, "dur"))
}

func TestTemporalParameters(t *testing.T) {
	s := clean(t)
	prague, err := time.LoadLocation("Europe/Prague")
	if err != nil {
		t.Skip("no time zone database:", err)
	}
	// The driver sends a time.Time whose zone is named "Offset" as an offset datetime.
	dt := time.Date(2001, 2, 3, 4, 5, 6, 0, time.FixedZone("Offset", -5*3600))
	params := map[string]any{
		"d":   neo4j.DateOf(time.Date(2001, 2, 3, 0, 0, 0, 0, time.UTC)),
		"ldt": neo4j.LocalDateTime(time.Date(2001, 2, 3, 4, 5, 6, 0, time.UTC)),
		"dt":  dt,
		"zdt": time.Date(2001, 2, 3, 4, 5, 6, 0, prague),
		"lt":  neo4j.LocalTime(time.Date(0, 1, 1, 7, 8, 9, 0, time.UTC)),
		"dur": neo4j.Duration{Days: 2, Seconds: 30},
	}
	rec := single(t, s,
		"RETURN toString($d) AS d, toString($ldt) AS ldt, toString($dt) AS dt,"+
			" toString($zdt) AS zdt, $dt.epochSeconds AS epoch, toString($lt) AS lt,"+
			" toString($dur) AS dur", params)
	eq(t, "2001-02-03", get(t, rec, "d"))
	eq(t, "2001-02-03T04:05:06", get(t, rec, "ldt"))
	eq(t, "2001-02-03T04:05:06-05:00", get(t, rec, "dt"))
	eq(t, "2001-02-03T04:05:06+01:00[Europe/Prague]", get(t, rec, "zdt"))
	eq(t, dt.Unix(), get(t, rec, "epoch"))
	eq(t, "07:08:09", get(t, rec, "lt"))
	eq(t, "P2DT30S", get(t, rec, "dur"))
	// Round trip unchanged.
	back := single(t, s, "RETURN $dt AS dt", params)
	if !get(t, back, "dt").(time.Time).Equal(dt) {
		t.Fatalf("round trip %v", get(t, back, "dt"))
	}
}

func TestSpatialValues(t *testing.T) {
	s := clean(t)
	rec := single(t, s,
		"RETURN point({x: 1.5, y: -2}) AS c, point({x: 1, y: 2, z: 3}) AS c3,"+
			" point({longitude: 14.42, latitude: 50.09}) AS g", nil)
	eq(t, neo4j.Point2D{X: 1.5, Y: -2, SpatialRefId: 7203}, get(t, rec, "c"))
	eq(t, neo4j.Point3D{X: 1, Y: 2, Z: 3, SpatialRefId: 9157}, get(t, rec, "c3"))
	eq(t, neo4j.Point2D{X: 14.42, Y: 50.09, SpatialRefId: 4326}, get(t, rec, "g"))
	r := single(t, s, "RETURN $p.x AS x, point.distance($p, point({x: 0, y: 0})) AS d",
		map[string]any{"p": neo4j.Point2D{X: 3, Y: 4, SpatialRefId: 7203}})
	eq(t, 3.0, get(t, r, "x"))
	eq(t, 5.0, get(t, r, "d"))
}

func TestGraphValues(t *testing.T) {
	s := clean(t)
	rec := single(t, s,
		"CREATE p = (a:Person:Admin {name: 'Ann', age: 30})-[r:KNOWS {since: 2020}]->"+
			"(b:Person {name: 'Bob'}) RETURN a, r, b, p", nil)
	a := get(t, rec, "a").(neo4j.Node)
	r := get(t, rec, "r").(neo4j.Relationship)
	b := get(t, rec, "b").(neo4j.Node)
	p := get(t, rec, "p").(neo4j.Path)
	labels := append([]string{}, a.Labels...)
	sort.Strings(labels)
	eq(t, []string{"Admin", "Person"}, labels)
	eq(t, map[string]any{"name": "Ann", "age": int64(30)}, a.Props)
	eq(t, "KNOWS", r.Type)
	eq(t, int64(2020), r.Props["since"])
	eq(t, a.ElementId, r.StartElementId)
	eq(t, b.ElementId, r.EndElementId)
	eq(t, 1, len(p.Relationships))
	eq(t, []any{"Ann", "Bob"}, []any{p.Nodes[0].Props["name"], p.Nodes[1].Props["name"]})
	// Element ids are stable and usable from Cypher.
	found := single(t, s, "MATCH (n) WHERE elementId(n) = $id RETURN n.name AS name",
		map[string]any{"id": b.ElementId})
	eq(t, "Bob", get(t, found, "name"))
}

func TestReverseRelationshipInPath(t *testing.T) {
	s := clean(t)
	mustConsume(t, s, "CREATE (:A {n: 1})-[:R]->(:B {n: 2})", nil)
	p := get(t, single(t, s, "MATCH p = (:B)<-[:R]-(:A) RETURN p", nil), "p").(neo4j.Path)
	eq(t, []any{int64(2), int64(1)}, []any{p.Nodes[0].Props["n"], p.Nodes[1].Props["n"]})
	rel := p.Relationships[0]
	eq(t, p.Nodes[1].ElementId, rel.StartElementId)
	eq(t, p.Nodes[0].ElementId, rel.EndElementId)
}

// --- results ----------------------------------------------------------------

func TestStreamingWithSmallFetchSize(t *testing.T) {
	s := clean(t, neo4j.SessionConfig{FetchSize: 7})
	res, err := s.Run(ctx, "UNWIND range(1, 1000) AS x RETURN x", nil)
	if err != nil {
		t.Fatal(err)
	}
	seen := []int64{}
	for res.Next(ctx) {
		seen = append(seen, res.Record().Values[0].(int64))
	}
	if err := res.Err(); err != nil {
		t.Fatal(err)
	}
	eq(t, rangeInts(1, 1000), seen)
}

func TestPartlyReadResults(t *testing.T) {
	s := clean(t, neo4j.SessionConfig{FetchSize: 10})
	first, err := s.Run(ctx, "UNWIND range(1, 100) AS x RETURN x", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !first.Next(ctx) {
		t.Fatal(first.Err())
	}
	eq(t, int64(1), first.Record().Values[0])
	// A new query in the same session discards the rest of the first.
	eq(t, int64(42), get(t, single(t, s, "RETURN 42 AS x", nil), "x"))
}

func TestSummaryCounters(t *testing.T) {
	s := clean(t)
	summary := mustConsume(t, s, "CREATE (a:L {p: 1})-[:T]->(b:L), (c:M) SET c.q = 2 RETURN a", nil)
	c := summary.Counters()
	eq(t, []int{3, 1, 3, 2}, []int{c.NodesCreated(), c.RelationshipsCreated(), c.LabelsAdded(), c.PropertiesSet()})
	eq(t, true, c.ContainsUpdates())
	eq(t, neo4j.StatementTypeReadWrite, summary.StatementType())
	summary = mustConsume(t, s, "MATCH (n) DETACH DELETE n", nil)
	eq(t, []int{3, 1}, []int{summary.Counters().NodesDeleted(), summary.Counters().RelationshipsDeleted()})
	eq(t, neo4j.StatementTypeReadOnly, mustConsume(t, s, "RETURN 1", nil).StatementType())
}

func TestSchemaCounters(t *testing.T) {
	s := clean(t)
	summary := mustConsume(t, s, "CREATE INDEX smoke_idx IF NOT EXISTS FOR (n:Smoke) ON (n.k)", nil)
	eq(t, 1, summary.Counters().IndexesAdded())
	eq(t, neo4j.StatementTypeSchemaWrite, summary.StatementType())
	summary = mustConsume(t, s, "DROP INDEX smoke_idx", nil)
	eq(t, 1, summary.Counters().IndexesRemoved())
}

func TestExplainAndProfile(t *testing.T) {
	s := clean(t)
	summary := mustConsume(t, s, "EXPLAIN MATCH (n) RETURN n", nil)
	if summary.Plan() == nil || summary.Plan().Operator() == "" {
		t.Fatal("no plan")
	}
	mustConsume(t, s, "CREATE (:X), (:X)", nil)
	summary = mustConsume(t, s, "PROFILE MATCH (n:X) RETURN n", nil)
	if summary.Profile() == nil {
		t.Fatal("no profile")
	}
	eq(t, int64(2), summary.Profile().Records())
}

func TestExecuteQueryAPI(t *testing.T) {
	clean(t)
	if _, err := neo4j.ExecuteQuery(ctx, driver, "CREATE (:Q {v: $v})", map[string]any{"v": 7}, neo4j.EagerResultTransformer); err != nil {
		t.Fatal(err)
	}
	r, err := neo4j.ExecuteQuery(ctx, driver, "MATCH (q:Q) RETURN q.v AS v", nil, neo4j.EagerResultTransformer)
	if err != nil {
		t.Fatal(err)
	}
	eq(t, []string{"v"}, r.Keys)
	eq(t, 1, len(r.Records))
	eq(t, int64(7), r.Records[0].Values[0])
	if r.Summary.Database().Name() == "" {
		t.Fatal("no database name in the summary")
	}
}

func TestRecordHelpers(t *testing.T) {
	s := clean(t)
	rec := single(t, s, "RETURN 1 AS a, 2 AS b", nil)
	eq(t, []any{int64(1), int64(2)}, rec.Values)
	b, isNil, err := neo4j.GetRecordValue[int64](rec, "b")
	if err != nil || isNil {
		t.Fatal(err, isNil)
	}
	eq(t, int64(2), b)
	m := rec.AsMap()
	eq(t, map[string]any{"a": int64(1), "b": int64(2)}, m)
	eq(t, []int64{1, 2, 3}, ints(t, s, "UNWIND [1, 2, 3] AS x RETURN x"))
	res, err := s.Run(ctx, "RETURN 1 AS x LIMIT 0", nil)
	if err != nil {
		t.Fatal(err)
	}
	recs, err := res.Collect(ctx)
	if err != nil {
		t.Fatal(err)
	}
	eq(t, 0, len(recs))
}

// --- transactions -----------------------------------------------------------

func TestManagedTransactions(t *testing.T) {
	s := clean(t)
	for _, name := range []string{"a", "b"} {
		_, err := s.ExecuteWrite(ctx, func(tx neo4j.ManagedTransaction) (any, error) {
			return consume(tx, "CREATE (:Acc {name: $n, bal: 100})", map[string]any{"n": name})
		})
		if err != nil {
			t.Fatal(err)
		}
	}
	total, err := neo4j.ExecuteRead(ctx, s, func(tx neo4j.ManagedTransaction) (int64, error) {
		res, err := tx.Run(ctx, "MATCH (a:Acc) RETURN sum(a.bal) AS s", nil)
		if err != nil {
			return 0, err
		}
		rec, err := res.Single(ctx)
		if err != nil {
			return 0, err
		}
		return rec.Values[0].(int64), nil
	})
	if err != nil {
		t.Fatal(err)
	}
	eq(t, int64(200), total)
}

func TestExplicitCommitAndRollback(t *testing.T) {
	s := clean(t)
	tx, err := s.BeginTransaction(ctx)
	if err != nil {
		t.Fatal(err)
	}
	mustConsume(t, tx, "CREATE (:T {v: 1})", nil)
	// Visible inside the transaction...
	eq(t, int64(1), get(t, single(t, tx, "MATCH (t:T) RETURN count(t) AS c", nil), "c"))
	if err := tx.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	tx, err = s.BeginTransaction(ctx)
	if err != nil {
		t.Fatal(err)
	}
	mustConsume(t, tx, "CREATE (:T {v: 2})", nil)
	if err := tx.Rollback(ctx); err != nil {
		t.Fatal(err)
	}
	tx, err = s.BeginTransaction(ctx)
	if err != nil {
		t.Fatal(err)
	}
	mustConsume(t, tx, "CREATE (:T {v: 3})", nil)
	if err := tx.Close(ctx); err != nil { // closed without commit: rolled back
		t.Fatal(err)
	}
	eq(t, []int64{1}, ints(t, s, "MATCH (t:T) RETURN t.v AS v ORDER BY v"))
}

func TestSeveralOpenResultsInOneTransaction(t *testing.T) {
	s := clean(t, neo4j.SessionConfig{FetchSize: 2})
	tx, err := s.BeginTransaction(ctx)
	if err != nil {
		t.Fatal(err)
	}
	r1, err := tx.Run(ctx, "UNWIND range(1, 5) AS x RETURN x", nil)
	if err != nil {
		t.Fatal(err)
	}
	r2, err := tx.Run(ctx, "UNWIND range(10, 15) AS y RETURN y", nil)
	if err != nil {
		t.Fatal(err)
	}
	collect := func(r neo4j.ResultWithContext) []int64 {
		recs, err := r.Collect(ctx)
		if err != nil {
			t.Fatal(err)
		}
		out := []int64{}
		for _, rec := range recs {
			out = append(out, rec.Values[0].(int64))
		}
		return out
	}
	eq(t, rangeInts(10, 15), collect(r2))
	eq(t, rangeInts(1, 5), collect(r1))
	if err := tx.Commit(ctx); err != nil {
		t.Fatal(err)
	}
}

func TestTransactionMetadataAndTimeoutConfig(t *testing.T) {
	s := clean(t)
	res, err := s.Run(ctx, "CREATE (:Meta) RETURN 1 AS x", nil,
		neo4j.WithTxMetadata(map[string]any{"app": "smoke"}), neo4j.WithTxTimeout(10*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	rec, err := res.Single(ctx)
	if err != nil {
		t.Fatal(err)
	}
	eq(t, int64(1), rec.Values[0])
	tx, err := s.BeginTransaction(ctx, neo4j.WithTxMetadata(map[string]any{"k": "v"}), neo4j.WithTxTimeout(10*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	mustConsume(t, tx, "RETURN 1", nil)
	if err := tx.Commit(ctx); err != nil {
		t.Fatal(err)
	}
}

func TestQueryTimeout(t *testing.T) {
	s := clean(t)
	res, err := s.Run(ctx, "UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*)", nil,
		neo4j.WithTxTimeout(200*time.Millisecond))
	if err == nil {
		_, err = res.Consume(ctx)
	}
	e := neoError(t, err)
	if !strings.Contains(e.Code, "Transaction") {
		t.Fatalf("code %q", e.Code)
	}
	// The session recovers.
	eq(t, int64(1), get(t, single(t, s, "RETURN 1 AS x", nil), "x"))
}

func TestBookmarksChainSessions(t *testing.T) {
	s1 := clean(t)
	mustConsume(t, s1, "CREATE (:Bm {v: 1})", nil)
	bookmarks := s1.LastBookmarks()
	s2 := session(t, neo4j.SessionConfig{Bookmarks: bookmarks})
	if c := get(t, single(t, s2, "MATCH (b:Bm) RETURN count(b) AS c", nil), "c").(int64); c < 1 {
		t.Fatalf("count %d", c)
	}
}

func TestBookmarkManager(t *testing.T) {
	clean(t)
	manager := neo4j.NewBookmarkManager(neo4j.BookmarkManagerConfig{})
	mustConsume(t, session(t, neo4j.SessionConfig{BookmarkManager: manager}), "CREATE (:Bm2)", nil)
	s := session(t, neo4j.SessionConfig{BookmarkManager: manager})
	eq(t, int64(1), get(t, single(t, s, "MATCH (b:Bm2) RETURN count(b) AS c", nil), "c"))
}

func TestConcurrentSessions(t *testing.T) {
	clean(t)
	var wg sync.WaitGroup
	errs := make(chan error, 8)
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			s := driver.NewSession(ctx, neo4j.SessionConfig{})
			defer s.Close(ctx)
			for j := 0; j < 20; j++ {
				_, err := s.ExecuteWrite(ctx, func(tx neo4j.ManagedTransaction) (any, error) {
					return consume(tx, "CREATE (:C {w: $i, j: $j})", map[string]any{"i": i, "j": j})
				})
				if err != nil {
					errs <- err
					return
				}
			}
		}(i)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Fatal(err)
	}
	r, err := neo4j.ExecuteQuery(ctx, driver, "MATCH (c:C) RETURN count(c) AS n", nil, neo4j.EagerResultTransformer)
	if err != nil {
		t.Fatal(err)
	}
	eq(t, int64(160), r.Records[0].Values[0])
}

// --- errors -----------------------------------------------------------------

func TestSyntaxError(t *testing.T) {
	s := clean(t)
	_, err := consume(s, "MATCH (n RETURN n", nil)
	eq(t, "Neo.ClientError.Statement.SyntaxError", neoError(t, err).Code)
	eq(t, int64(1), get(t, single(t, s, "RETURN 1 AS x", nil), "x"))
}

func TestConstraintViolation(t *testing.T) {
	s := clean(t)
	mustConsume(t, s, "CREATE CONSTRAINT smoke_u IF NOT EXISTS FOR (u:U) REQUIRE u.id IS UNIQUE", nil)
	defer mustConsume(t, s, "DROP CONSTRAINT smoke_u", nil)
	mustConsume(t, s, "CREATE (:U {id: 1})", nil)
	_, err := consume(s, "CREATE (:U {id: 1})", nil)
	eq(t, "Neo.ClientError.Schema.ConstraintValidationFailed", neoError(t, err).Code)
	_, err = s.ExecuteWrite(ctx, func(tx neo4j.ManagedTransaction) (any, error) {
		return consume(tx, "CREATE (:U {id: 1})", nil)
	})
	eq(t, "Neo.ClientError.Schema.ConstraintValidationFailed", neoError(t, err).Code)
	eq(t, int64(1), get(t, single(t, s, "MATCH (u:U) RETURN count(u) AS c", nil), "c"))
}

func TestErrorInsideTransactionRollsBack(t *testing.T) {
	s := clean(t)
	tx, err := s.BeginTransaction(ctx)
	if err != nil {
		t.Fatal(err)
	}
	mustConsume(t, tx, "CREATE (:E)", nil)
	_, err = consume(tx, "RETURN 1 / 0", nil)
	if !strings.HasPrefix(neoError(t, err).Code, "Neo.ClientError.") {
		t.Fatalf("code %q", neoError(t, err).Code)
	}
	_ = tx.Close(ctx)
	eq(t, int64(0), get(t, single(t, s, "MATCH (e:E) RETURN count(e) AS c", nil), "c"))
}

func TestUnknownDatabase(t *testing.T) {
	s := session(t, neo4j.SessionConfig{DatabaseName: "no_such_db"})
	_, err := consume(s, "RETURN 1", nil)
	eq(t, "Neo.ClientError.Database.DatabaseNotFound", neoError(t, err).Code)
}

// --- databases --------------------------------------------------------------

func TestOtherDatabase(t *testing.T) {
	clean(t)
	q := func(cypher, database string) *neo4j.EagerResult {
		t.Helper()
		opts := []neo4j.ExecuteQueryConfigurationOption{}
		if database != "" {
			opts = append(opts, neo4j.ExecuteQueryWithDatabase(database))
		}
		r, err := neo4j.ExecuteQuery(ctx, driver, cypher, nil, neo4j.EagerResultTransformer, opts...)
		if err != nil {
			t.Fatal(err)
		}
		return r
	}
	// Administration goes through the `system` database, as with Neo4j.
	q("CREATE DATABASE smoke2 IF NOT EXISTS", "system")
	defer q("DROP DATABASE smoke2 IF EXISTS", "system")
	names := []any{}
	for _, r := range q("SHOW DATABASES", "system").Records {
		name, _ := r.Get("name")
		names = append(names, name)
	}
	found := false
	for _, n := range names {
		found = found || n == "smoke2"
	}
	if !found {
		t.Fatalf("smoke2 not in %v", names)
	}
	q("CREATE (:OnlyHere)", "smoke2")
	eq(t, int64(1), q("MATCH (n:OnlyHere) RETURN count(n) AS c", "smoke2").Records[0].Values[0])
	eq(t, int64(0), q("MATCH (n:OnlyHere) RETURN count(n) AS c", "").Records[0].Values[0])
	s := session(t, neo4j.SessionConfig{DatabaseName: "smoke2"})
	eq(t, int64(1), get(t, single(t, s, "MATCH (n) RETURN count(n) AS c", nil), "c"))
}

func TestSystemDatabaseRejectsQueries(t *testing.T) {
	s := session(t, neo4j.SessionConfig{DatabaseName: "system"})
	_, err := consume(s, "MATCH (n) RETURN n", nil)
	if !strings.HasPrefix(neoError(t, err).Code, "Neo.ClientError.") {
		t.Fatalf("code %q", neoError(t, err).Code)
	}
}
