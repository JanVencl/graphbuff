# Full-text search

A full-text index splits the text of string properties into words and finds the entities
that match a search query, ranked by relevance. The Cypher surface is the one Neo4j 5
uses (`CREATE FULLTEXT INDEX`, `db.index.fulltext.queryNodes`), so existing queries and
drivers work unchanged.

- [Full-text indexes](#full-text-indexes)
- [Querying an index](#querying-an-index)
- [Query syntax](#query-syntax)
- [Analyzers](#analyzers)
- [What gets indexed](#what-gets-indexed)
- [How it works](#how-it-works)
- [Limits](#limits)

## Full-text indexes

```cypher
CREATE FULLTEXT INDEX titles IF NOT EXISTS
FOR (n:Movie|Book) ON EACH [n.title, n.plot]
OPTIONS {indexConfig: {`fulltext.analyzer`: 'english'}}
```

- One or more labels (`n:A|B`) and one or more properties (`ON EACH [n.a, n.b]`).
- `FOR ()-[r:A|B]-() ON EACH [r.prop]` indexes relationships of those types instead.
- `fulltext.analyzer` picks the [analyzer](#analyzers) (`standard-no-stop-words` by
  default). `fulltext.eventually_consistent` is accepted and ignored: the index is always
  up to date when a transaction commits. `indexProvider` is accepted and ignored; any
  other option is an error.
- Without a name, the index is called `fulltext_<Label>_<prop>` (all labels and
  properties, joined by `_`).
- A full-text index and a range index may sit on the same property.
- `SHOW INDEXES` and `CALL db.indexes()` list it with type `FULLTEXT`, its labels as
  `A|B`, and its options; `DROP INDEX name` removes it.
- Index definitions are kept by every storage mode, and creating or dropping one is part
  of its transaction.

## Querying an index

```cypher
CALL db.index.fulltext.queryNodes('titles', 'matrix OR "blade runner"')
YIELD node, score
RETURN node.title, score
```

`db.index.fulltext.queryRelationships(name, query)` yields `relationship, score`.

- Results come best score first; equal scores in id order.
- A third argument holds options: `skip` and `limit` (non-negative integers) page the
  results, and `analyzer` analyzes the query with another analyzer than the index's.
  Every match is scored, with or without `limit`.
- An unknown index, a node index queried for relationships (or the other way round), or a
  query that does not parse is an error.
- Inside a transaction that has written, the query sees those writes.
- `db.index.fulltext.listAvailableAnalyzers()` yields `analyzer, description, stopwords`.
  `db.index.fulltext.awaitEventuallyConsistentIndexRefresh()` returns at once.

## Query syntax

The query string uses Lucene's classic syntax:

| Query | Matches |
|-------|---------|
| `matrix reloaded` | either word (the default operator is `OR`) |
| `matrix AND reloaded`, `+matrix +reloaded` | both words |
| `matrix -reloaded`, `matrix NOT reloaded` | `matrix` but not `reloaded` |
| `"matrix reloaded"` | the words next to each other, in that order |
| `title:matrix`, `title:(a OR b)` | only in property `title` |
| `matr*`, `mat?ix` | wildcards: `*` any characters, `?` one |
| `matirx~`, `matirx~1` | fuzzy: within 2 (or the given number of) edits |
| `speed^3 OR hacker` | `^n` weights a clause's score |
| `*:*` | every indexed entity |

- `&&`, `||` and `!` work as `AND`, `OR` and `NOT`. `AND`, `OR` and `NOT` must be
  uppercase; lowercase they are ordinary words.
- Words and phrases are analyzed with the index's analyzer, so case and (for `english`)
  word endings do not matter. Wildcard and fuzzy terms are lowercased but not stemmed.
- A clause of stop words only (`the` with the `standard` analyzer) is dropped; a query of
  only negative clauses (`NOT matrix`) matches nothing.
- Escape special characters with `\`: `+ - && || ! ( ) { } [ ] ^ " ~ * ? : \ /`.
- Not supported (an `Unsupported` error): ranges (`[a TO b]`), regular expressions
  (`/ab+/`) and phrase slop (`"a b"~2`).

Scores are BM25, as Lucene computes them (k1 = 1.2, b = 0.75, per property): rarer words,
more occurrences and shorter values score higher, and the scores of the clauses that match
add up. Wildcard matches score 1 per property. Rankings agree with Neo4j's in practice;
the exact numbers can differ.

## Analyzers

| Analyzer | Terms |
|----------|-------|
| `standard-no-stop-words` (default) | words at Unicode word boundaries, lowercased |
| `standard` | as above, without English stop words |
| `english` | as `standard`, also without possessive `'s`, stemmed with Porter's algorithm (`running` → `run`) |
| `simple` | runs of letters, lowercased |
| `stop` | as `simple`, without English stop words |
| `whitespace` | runs of non-whitespace characters, as written |
| `keyword` | the whole value as one term, as written |

Words keep inner apostrophes, periods and digit separators (`don't`, `u.s.a`, `3.14`);
Chinese and Japanese ideographs are one term each. The stop words are Lucene's 33 English
ones (`a`, `and`, `the`, `of`, …).

## What gets indexed

Nodes with any of the labels (or relationships of any of the types) and at least one
indexed property that is a STRING or a LIST of STRINGs. Other values are skipped silently:
writes never fail because of a full-text index.

## How it works

- The index keeps an inverted index (term → entities and counts, plus each entity's terms
  in order for phrases) in memory, built from the committed data on its first query.
- Each commit updates only the entities it changed, so the index stays current without
  rebuilding.
- Inside a transaction, a query builds the index from what the transaction sees.

## Limits

- The in-memory index is rebuilt after a restart, on each index's first query; it takes
  memory in proportion to the indexed text.
- Building a relationship index scans all relationships.
- Analyzers other than the seven above (language-specific ones, `url_or_email`, custom
  analyzers) are not available.
