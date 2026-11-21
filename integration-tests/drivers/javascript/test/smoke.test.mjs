// Smoke suite: the official Neo4j JavaScript driver against graphbuff-server over Bolt.

import assert from 'node:assert/strict'
import { after, before, beforeEach, describe, test } from 'node:test'
import neo4j from 'neo4j-driver'
import { startServer, TOKEN } from './server.mjs'

const { int, isInt } = neo4j
const auth = neo4j.auth.basic('neo4j', TOKEN)

let server
let driver

before(async () => {
  server = await startServer()
  driver = neo4j.driver(server.bolt, auth)
})

after(async () => {
  await driver?.close()
  await server?.stop()
})

async function withSession (fn, config = {}) {
  const s = driver.session(config)
  try {
    return await fn(s)
  } finally {
    await s.close()
  }
}

async function single (s, query, params) {
  const { records } = await s.run(query, params)
  assert.equal(records.length, 1)
  return records[0]
}

async function values (s, query, params) {
  const { records } = await s.run(query, params)
  return records.map(r => num(r.get(0)))
}

// [major, minor]: driver 5.x reports a number (5.8), 6.x a ProtocolVersion object.
function protocol (info) {
  const v = info.protocolVersion
  if (typeof v === 'number') {
    const [major, minor] = v.toFixed(1).split('.').map(Number)
    return [major, minor]
  }
  // The fields, not getMinor(): in 6.3.2 that returns the major version.
  return [v.major, v.minor]
}

// Integers come back as neo4j Integer by default.
function num (v) {
  return isInt(v) ? v.toNumber() : v
}

async function expectError (promise, check) {
  await assert.rejects(promise, e => {
    assert.ok(e instanceof neo4j.Neo4jError, `expected Neo4jError, got ${e}`)
    if (check) check(e)
    return true
  })
}

beforeEach(async () => {
  await driver.executeQuery('MATCH (n) DETACH DELETE n')
})

// --- connection -------------------------------------------------------------

describe('connection', () => {
  test('verify connectivity and server info', async () => {
    const info = await driver.getServerInfo()
    assert.match(info.agent, /^Neo4j\//)
    const [major, minor] = protocol(info)
    assert.ok(major > 4 || (major === 4 && minor >= 4))
  })

  test('newest Bolt and GQL statuses', async () => {
    const info = await driver.getServerInfo()
    // The newest Bolt both speak: 5.8 for drivers that know it (5.28 and 6.x).
    assert.deepEqual(protocol(info), [5, 8])
    await withSession(async s => {
      let summary = (await s.run('UNWIND [1, 2] AS x RETURN x')).summary
      assert.equal(summary.gqlStatusObjects[0].gqlStatus, '00000')
      summary = (await s.run('UNWIND [] AS x RETURN x')).summary
      assert.equal(summary.gqlStatusObjects[0].gqlStatus, '02000')
      await expectError(s.run('RETURN 1 +'), e => {
        assert.equal(e.code, 'Neo.ClientError.Statement.SyntaxError')
        assert.equal(e.gqlStatus, '42001')
      })
    })
  })

  test('routing scheme', async () => {
    const d = neo4j.driver(server.neo4j, auth)
    try {
      await d.verifyConnectivity()
      let { records } = await d.executeQuery('RETURN 1 AS x', {}, { routing: neo4j.routing.READ })
      assert.equal(num(records[0].get('x')), 1)
      ;({ records } = await d.executeQuery('RETURN 2 AS x', {}, { routing: neo4j.routing.WRITE }))
      assert.equal(num(records[0].get('x')), 2)
    } finally {
      await d.close()
    }
  })

  test('wrong password', async () => {
    const d = neo4j.driver(server.bolt, neo4j.auth.basic('neo4j', 'wrong'))
    try {
      await expectError(d.verifyConnectivity(), e => {
        assert.equal(e.code, 'Neo.ClientError.Security.Unauthorized')
      })
    } finally {
      await d.close()
    }
  })

  test('bearer auth', async () => {
    const d = neo4j.driver(server.bolt, neo4j.auth.bearer(TOKEN))
    try {
      await d.verifyConnectivity()
    } finally {
      await d.close()
    }
  })
})

// --- values -----------------------------------------------------------------

describe('values', () => {
  test('scalar values', () => withSession(async s => {
    const rec = await single(s,
      "RETURN 1 AS i, -9007199254740993 AS big, 1.5 AS f, 'héllo' AS s, true AS b," +
      ' null AS n, [1, \'a\', null] AS l, {k: [1, {x: 2}]} AS m')
    assert.ok(isInt(rec.get('i')))
    assert.equal(rec.get('i').toNumber(), 1)
    // Beyond Number.MAX_SAFE_INTEGER: exact through Integer.
    assert.equal(rec.get('big').toString(), '-9007199254740993')
    assert.equal(rec.get('f'), 1.5)
    assert.equal(rec.get('s'), 'héllo')
    assert.equal(rec.get('b'), true)
    assert.equal(rec.get('n'), null)
    assert.deepEqual(rec.get('l').map(num), [1, 'a', null])
    assert.equal(num(rec.get('m').k[1].x), 2)
    assert.deepEqual(rec.keys, ['i', 'big', 'f', 's', 'b', 'n', 'l', 'm'])
  }))

  test('BigInt results', async () => {
    const d = neo4j.driver(server.bolt, auth, { useBigInt: true })
    try {
      const { records } = await d.executeQuery('RETURN 9223372036854775807 AS max, -9223372036854775808 AS min')
      assert.equal(records[0].get('max'), 9223372036854775807n)
      assert.equal(records[0].get('min'), -9223372036854775808n)
    } finally {
      await d.close()
    }
  })

  test('parameters round trip', () => withSession(async s => {
    const params = {
      i: int('4611686018427387904'),
      bi: 2n ** 62n,
      num: 7, // a JS number is sent as a float
      f: -0.25,
      s: 'x'.repeat(70000),
      b: false,
      n: null,
      l: Array.from({ length: 300 }, (_, k) => int(k)),
      m: { nested: { deep: ['a', int(1), 2.5] } },
      el: [],
      em: {}
    }
    const rec = await single(s,
      'RETURN $i AS i, $bi AS bi, $num AS num, valueType($num) AS numType, $f AS f, $s AS s,' +
      ' $b AS b, $n AS n, $l AS l, $m AS m, $el AS el, $em AS em', params)
    assert.equal(rec.get('i').toString(), '4611686018427387904')
    assert.equal(rec.get('bi').toString(), '4611686018427387904')
    assert.equal(rec.get('num'), 7)
    assert.match(rec.get('numType'), /^FLOAT/)
    assert.equal(rec.get('f'), -0.25)
    assert.equal(rec.get('s'), params.s)
    assert.equal(rec.get('b'), false)
    assert.equal(rec.get('n'), null)
    assert.deepEqual(rec.get('l').map(num), params.l.map(num))
    assert.deepEqual(rec.get('m').nested.deep.map(num), ['a', 1, 2.5])
    assert.deepEqual(rec.get('el'), [])
    assert.deepEqual(rec.get('em'), {})
  }))

  test('temporal values', () => withSession(async s => {
    const rec = await single(s,
      "RETURN date('2024-02-29') AS d, localtime('12:34:56.789') AS lt," +
      " time('12:00:00+02:00') AS t, localdatetime('2024-01-02T03:04:05') AS ldt," +
      " datetime('2024-01-02T03:04:05.123456789+01:00') AS dt," +
      " datetime('2024-01-02T03:04:05[Europe/Prague]') AS zdt," +
      " duration('P1Y2M3DT4H5M6.5S') AS dur")
    assert.ok(neo4j.isDate(rec.get('d')))
    assert.equal(rec.get('d').toString(), '2024-02-29')
    assert.ok(neo4j.isLocalTime(rec.get('lt')))
    assert.equal(rec.get('lt').toString(), '12:34:56.789000000')
    assert.ok(neo4j.isTime(rec.get('t')))
    assert.equal(num(rec.get('t').timeZoneOffsetSeconds), 7200)
    assert.ok(neo4j.isLocalDateTime(rec.get('ldt')))
    assert.equal(rec.get('ldt').toString(), '2024-01-02T03:04:05')
    const dt = rec.get('dt')
    assert.ok(neo4j.isDateTime(dt))
    assert.equal(num(dt.nanosecond), 123456789)
    assert.equal(num(dt.timeZoneOffsetSeconds), 3600)
    assert.equal(dt.toStandardDate().toISOString(), '2024-01-02T02:04:05.123Z')
    const zdt = rec.get('zdt')
    assert.equal(zdt.timeZoneId, 'Europe/Prague')
    assert.equal(num(zdt.hour), 3)
    const dur = rec.get('dur')
    assert.ok(neo4j.isDuration(dur))
    assert.equal(num(dur.months), 14)
    assert.equal(num(dur.days), 3)
    assert.equal(num(dur.seconds), 4 * 3600 + 5 * 60 + 6)
    assert.equal(num(dur.nanoseconds), 500000000)
  }))

  test('temporal parameters', () => withSession(async s => {
    const jsDate = new Date('2001-02-03T04:05:06Z')
    const params = {
      d: new neo4j.types.Date(2001, 2, 3),
      ldt: new neo4j.types.LocalDateTime(2001, 2, 3, 4, 5, 6, 0),
      dt: new neo4j.types.DateTime(2001, 2, 3, 4, 5, 6, 0, -5 * 3600),
      zdt: new neo4j.types.DateTime(2001, 2, 3, 4, 5, 6, 0, undefined, 'Europe/Prague'),
      std: neo4j.types.DateTime.fromStandardDate(jsDate),
      lt: new neo4j.types.LocalTime(7, 8, 9, 0),
      dur: new neo4j.types.Duration(0, 2, 30, 0)
    }
    const rec = await single(s,
      'RETURN toString($d) AS d, toString($ldt) AS ldt, toString($dt) AS dt,' +
      ' toString($zdt) AS zdt, $std.epochSeconds AS epoch, toString($lt) AS lt,' +
      ' toString($dur) AS dur', params)
    assert.equal(rec.get('d'), '2001-02-03')
    assert.equal(rec.get('ldt'), '2001-02-03T04:05:06')
    assert.equal(rec.get('dt'), '2001-02-03T04:05:06-05:00')
    assert.equal(rec.get('zdt'), '2001-02-03T04:05:06+01:00[Europe/Prague]')
    assert.equal(num(rec.get('epoch')), jsDate.getTime() / 1000)
    assert.equal(rec.get('lt'), '07:08:09')
    assert.equal(rec.get('dur'), 'P2DT30S')
    // Round trip unchanged.
    const back = await single(s, 'RETURN $dt AS dt, $d AS d', params)
    assert.equal(back.get('dt').toString(), params.dt.toString())
    assert.equal(back.get('d').toString(), '2001-02-03')
  }))

  test('spatial values', () => withSession(async s => {
    const rec = await single(s,
      'RETURN point({x: 1.5, y: -2}) AS c, point({x: 1, y: 2, z: 3}) AS c3,' +
      ' point({longitude: 14.42, latitude: 50.09}) AS g')
    const c = rec.get('c')
    assert.ok(neo4j.isPoint(c))
    assert.deepEqual([num(c.srid), c.x, c.y], [7203, 1.5, -2])
    const c3 = rec.get('c3')
    assert.deepEqual([num(c3.srid), c3.x, c3.y, c3.z], [9157, 1, 2, 3])
    const g = rec.get('g')
    assert.deepEqual([num(g.srid), g.x, g.y], [4326, 14.42, 50.09])
    const r = await single(s, 'RETURN $p.x AS x, point.distance($p, point({x: 0, y: 0})) AS d',
      { p: new neo4j.types.Point(7203, 3, 4) })
    assert.equal(r.get('x'), 3)
    assert.equal(r.get('d'), 5)
  }))

  test('graph values', () => withSession(async s => {
    const rec = await single(s,
      "CREATE p = (a:Person:Admin {name: 'Ann', age: 30})-[r:KNOWS {since: 2020}]->" +
      "(b:Person {name: 'Bob'}) RETURN a, r, b, p")
    const a = rec.get('a')
    const r = rec.get('r')
    const b = rec.get('b')
    const p = rec.get('p')
    assert.ok(neo4j.isNode(a))
    assert.deepEqual([...a.labels].sort(), ['Admin', 'Person'])
    assert.equal(a.properties.name, 'Ann')
    assert.equal(num(a.properties.age), 30)
    assert.ok(neo4j.isRelationship(r))
    assert.equal(r.type, 'KNOWS')
    assert.equal(num(r.properties.since), 2020)
    assert.equal(r.startNodeElementId, a.elementId)
    assert.equal(r.endNodeElementId, b.elementId)
    assert.ok(neo4j.isPath(p))
    assert.equal(p.length, 1)
    assert.equal(p.start.properties.name, 'Ann')
    assert.equal(p.end.properties.name, 'Bob')
    // Element ids are stable and usable from Cypher.
    const found = await single(s, 'MATCH (n) WHERE elementId(n) = $id RETURN n.name AS name',
      { id: b.elementId })
    assert.equal(found.get('name'), 'Bob')
  }))

  test('reverse relationship in path', () => withSession(async s => {
    await s.run('CREATE (:A {n: 1})-[:R]->(:B {n: 2})')
    const p = (await single(s, 'MATCH p = (:B)<-[:R]-(:A) RETURN p')).get('p')
    assert.equal(num(p.start.properties.n), 2)
    assert.equal(num(p.end.properties.n), 1)
    const seg = p.segments[0]
    assert.equal(num(seg.start.properties.n), 2)
    assert.equal(seg.relationship.startNodeElementId, seg.end.elementId)
    assert.equal(seg.relationship.endNodeElementId, seg.start.elementId)
  }))
})

// --- results ----------------------------------------------------------------

describe('results', () => {
  test('streaming with a small fetch size', () => withSession(async s => {
    const seen = []
    await new Promise((resolve, reject) => {
      s.run('UNWIND range(1, 1000) AS x RETURN x').subscribe({
        onNext: r => seen.push(num(r.get('x'))),
        onCompleted: resolve,
        onError: reject
      })
    })
    assert.deepEqual(seen, Array.from({ length: 1000 }, (_, k) => k + 1))
  }, { fetchSize: 7 }))

  test('async iteration and partly read results', () => withSession(async s => {
    const result = s.run('UNWIND range(1, 100) AS x RETURN x')
    for await (const r of result) {
      assert.equal(num(r.get('x')), 1)
      break
    }
    // A new query in the same session discards the rest of the first.
    assert.equal(num((await single(s, 'RETURN 42 AS x')).get('x')), 42)
  }, { fetchSize: 10 }))

  test('summary counters', () => withSession(async s => {
    let summary = (await s.run('CREATE (a:L {p: 1})-[:T]->(b:L), (c:M) SET c.q = 2 RETURN a')).summary
    const c = summary.counters.updates()
    assert.equal(c.nodesCreated, 3)
    assert.equal(c.relationshipsCreated, 1)
    assert.equal(c.labelsAdded, 3)
    assert.equal(c.propertiesSet, 2)
    assert.ok(summary.counters.containsUpdates())
    assert.equal(summary.queryType, 'rw')
    summary = (await s.run('MATCH (n) DETACH DELETE n')).summary
    assert.equal(summary.counters.updates().nodesDeleted, 3)
    assert.equal(summary.counters.updates().relationshipsDeleted, 1)
    assert.equal((await s.run('RETURN 1')).summary.queryType, 'r')
  }))

  test('schema counters', () => withSession(async s => {
    let summary = (await s.run('CREATE INDEX smoke_idx IF NOT EXISTS FOR (n:Smoke) ON (n.k)')).summary
    assert.equal(summary.counters.updates().indexesAdded, 1)
    assert.equal(summary.queryType, 's')
    summary = (await s.run('DROP INDEX smoke_idx')).summary
    assert.equal(summary.counters.updates().indexesRemoved, 1)
  }))

  test('EXPLAIN and PROFILE', () => withSession(async s => {
    let summary = (await s.run('EXPLAIN MATCH (n) RETURN n')).summary
    assert.ok(summary.hasPlan())
    assert.ok(summary.plan.operatorType)
    await s.run('CREATE (:X), (:X)')
    summary = (await s.run('PROFILE MATCH (n:X) RETURN n')).summary
    assert.ok(summary.hasProfile())
    assert.equal(summary.profile.rows, 2)
  }))

  test('executeQuery API', async () => {
    await driver.executeQuery('CREATE (:Q {v: $v})', { v: int(7) })
    const { records, summary, keys } = await driver.executeQuery('MATCH (q:Q) RETURN q.v AS v')
    assert.deepEqual(keys, ['v'])
    assert.deepEqual(records.map(r => num(r.get('v'))), [7])
    assert.ok(summary.database.name)
    const objects = await driver.executeQuery('RETURN 1 AS a, 2 AS b', {},
      { resultTransformer: neo4j.resultTransformers.mappedResultTransformer({ map: r => r.toObject() }) })
    assert.deepEqual(objects.records.map(o => [num(o.a), num(o.b)]), [[1, 2]])
  })

  test('record helpers', () => withSession(async s => {
    const rec = await single(s, 'RETURN 1 AS a, 2 AS b')
    const obj = rec.toObject()
    assert.deepEqual([num(obj.a), num(obj.b)], [1, 2])
    assert.ok(rec.has('a'))
    assert.deepEqual(await values(s, 'UNWIND [1, 2, 3] AS x RETURN x'), [1, 2, 3])
    assert.deepEqual((await s.run('RETURN 1 AS x LIMIT 0')).records, [])
  }))
})

// --- transactions -----------------------------------------------------------

describe('transactions', () => {
  test('managed transactions', () => withSession(async s => {
    for (const name of ['a', 'b']) {
      await s.executeWrite(tx => tx.run('CREATE (:Acc {name: $n, bal: 100})', { n: name }))
    }
    const total = await s.executeRead(async tx =>
      num((await tx.run('MATCH (a:Acc) RETURN sum(a.bal) AS s')).records[0].get('s')))
    assert.equal(total, 200)
  }))

  test('explicit commit and rollback', () => withSession(async s => {
    let tx = s.beginTransaction()
    await tx.run('CREATE (:T {v: 1})')
    // Visible inside the transaction...
    assert.equal(num((await tx.run('MATCH (t:T) RETURN count(t) AS c')).records[0].get('c')), 1)
    await tx.commit()
    tx = s.beginTransaction()
    await tx.run('CREATE (:T {v: 2})')
    await tx.rollback()
    tx = s.beginTransaction()
    await tx.run('CREATE (:T {v: 3})')
    await tx.close() // closed without commit: rolled back
    assert.equal(tx.isOpen(), false)
    assert.deepEqual(await values(s, 'MATCH (t:T) RETURN t.v AS v ORDER BY v'), [1])
  }))

  test('several open results in one transaction', () => withSession(async s => {
    const tx = s.beginTransaction()
    const r1 = tx.run('UNWIND range(1, 5) AS x RETURN x')
    const r2 = tx.run('UNWIND range(10, 15) AS y RETURN y')
    assert.deepEqual((await r2).records.map(r => num(r.get('y'))), [10, 11, 12, 13, 14, 15])
    assert.deepEqual((await r1).records.map(r => num(r.get('x'))), [1, 2, 3, 4, 5])
    await tx.commit()
  }, { fetchSize: 2 }))

  test('transaction metadata and timeout config', () => withSession(async s => {
    const rec = await s.run('CREATE (:Meta) RETURN 1 AS x', {}, { metadata: { app: 'smoke' }, timeout: 10_000 })
    assert.equal(num(rec.records[0].get('x')), 1)
    const tx = s.beginTransaction({ metadata: { k: 'v' }, timeout: 10_000 })
    await tx.run('RETURN 1')
    await tx.commit()
  }))

  test('query timeout', () => withSession(async s => {
    await expectError(
      s.run('UNWIND range(1, 100000) AS a UNWIND range(1, 100000) AS b RETURN count(*)', {}, { timeout: 200 }),
      e => assert.match(e.code, /Transaction/))
    // The session recovers.
    assert.equal(num((await single(s, 'RETURN 1 AS x')).get('x')), 1)
  }))

  test('bookmarks chain sessions', async () => {
    const bookmarks = await withSession(async s => {
      await s.run('CREATE (:Bm {v: 1})')
      return s.lastBookmarks()
    })
    await withSession(async s => {
      assert.ok(num((await single(s, 'MATCH (b:Bm) RETURN count(b) AS c')).get('c')) >= 1)
    }, { bookmarks })
  })

  test('bookmark manager', async () => {
    const bookmarkManager = neo4j.bookmarkManager()
    await withSession(s => s.run('CREATE (:Bm2)'), { bookmarkManager })
    await withSession(async s => {
      assert.equal(num((await single(s, 'MATCH (b:Bm2) RETURN count(b) AS c')).get('c')), 1)
    }, { bookmarkManager })
  })

  test('concurrent sessions', async () => {
    await Promise.all(Array.from({ length: 8 }, (_, i) => withSession(async s => {
      for (let j = 0; j < 20; j++) {
        await s.executeWrite(tx => tx.run('CREATE (:C {w: $i, j: $j})', { i: int(i), j: int(j) }))
      }
    })))
    const { records } = await driver.executeQuery('MATCH (c:C) RETURN count(c) AS n')
    assert.equal(num(records[0].get('n')), 160)
  })
})

// --- errors -----------------------------------------------------------------

describe('errors', () => {
  test('syntax error', () => withSession(async s => {
    await expectError(s.run('MATCH (n RETURN n'),
      e => assert.equal(e.code, 'Neo.ClientError.Statement.SyntaxError'))
    assert.equal(num((await single(s, 'RETURN 1 AS x')).get('x')), 1)
  }))

  test('constraint violation', () => withSession(async s => {
    await s.run('CREATE CONSTRAINT smoke_u IF NOT EXISTS FOR (u:U) REQUIRE u.id IS UNIQUE')
    try {
      await s.run('CREATE (:U {id: 1})')
      const isConstraint = e => assert.equal(e.code, 'Neo.ClientError.Schema.ConstraintValidationFailed')
      await expectError(s.run('CREATE (:U {id: 1})'), isConstraint)
      await expectError(s.executeWrite(tx => tx.run('CREATE (:U {id: 1})')), isConstraint)
      assert.equal(num((await single(s, 'MATCH (u:U) RETURN count(u) AS c')).get('c')), 1)
    } finally {
      await s.run('DROP CONSTRAINT smoke_u')
    }
  }))

  test('error inside a transaction rolls back', () => withSession(async s => {
    const tx = s.beginTransaction()
    await tx.run('CREATE (:E)')
    await expectError(tx.run('RETURN 1 / 0'), e => assert.match(e.code, /^Neo\.ClientError\./))
    await tx.close()
    assert.equal(num((await single(s, 'MATCH (e:E) RETURN count(e) AS c')).get('c')), 0)
  }))

  test('unknown database', () => withSession(async s => {
    await expectError(s.run('RETURN 1'),
      e => assert.equal(e.code, 'Neo.ClientError.Database.DatabaseNotFound'))
  }, { database: 'no_such_db' }))
})

// --- databases --------------------------------------------------------------

describe('databases', () => {
  test('other database', async () => {
    // Administration goes through the `system` database, as with Neo4j.
    await driver.executeQuery('CREATE DATABASE smoke2 IF NOT EXISTS', {}, { database: 'system' })
    try {
      const { records } = await driver.executeQuery('SHOW DATABASES', {}, { database: 'system' })
      assert.ok(records.map(r => r.get('name')).includes('smoke2'))
      await driver.executeQuery('CREATE (:OnlyHere)', {}, { database: 'smoke2' })
      let r = await driver.executeQuery('MATCH (n:OnlyHere) RETURN count(n) AS c', {}, { database: 'smoke2' })
      assert.equal(num(r.records[0].get('c')), 1)
      r = await driver.executeQuery('MATCH (n:OnlyHere) RETURN count(n) AS c')
      assert.equal(num(r.records[0].get('c')), 0)
      await withSession(async s => {
        assert.equal(num((await single(s, 'MATCH (n) RETURN count(n) AS c')).get('c')), 1)
      }, { database: 'smoke2' })
    } finally {
      await driver.executeQuery('DROP DATABASE smoke2 IF EXISTS', {}, { database: 'system' })
    }
  })

  test('system database rejects queries', () => withSession(async s => {
    await expectError(s.run('MATCH (n) RETURN n'), e => assert.match(e.code, /^Neo\.ClientError\./))
  }, { database: 'system' }))
})

// --- reactive API -----------------------------------------------------------

describe('reactive', () => {
  test('rx session streams records', async () => {
    const rx = driver.rxSession({ fetchSize: 5 })
    try {
      const seen = await new Promise((resolve, reject) => {
        const out = []
        rx.run('UNWIND range(1, 20) AS x RETURN x').records().subscribe({
          next: r => out.push(num(r.get('x'))),
          complete: () => resolve(out),
          error: reject
        })
      })
      assert.deepEqual(seen, Array.from({ length: 20 }, (_, k) => k + 1))
    } finally {
      await new Promise((resolve, reject) => rx.close().subscribe({ complete: resolve, error: reject }))
    }
  })
})
