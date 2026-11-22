//! The built-in functions, for `SHOW FUNCTIONS` and `dbms.functions()`: name, category,
//! signature and description. Names are matched without regard to case when a query
//! calls them; the catalogue gives each its usual spelling.

use graphbuff_core::Value;

/// `(name, category, signature, description)`.
type Entry = (&'static str, &'static str, &'static str, &'static str);

/// Every built-in function. A test checks that each one exists.
pub(crate) const FUNCTIONS: &[Entry] = &[
    // ---- aggregating ----
    ("avg", "Aggregating", "avg(input :: INTEGER | FLOAT | DURATION) :: INTEGER | FLOAT | DURATION", "The average of a set of numbers or durations."),
    ("collect", "Aggregating", "collect(input :: ANY) :: LIST<ANY>", "A list of the values, without nulls."),
    ("count", "Aggregating", "count(input :: ANY) :: INTEGER", "The number of values or rows."),
    ("max", "Aggregating", "max(input :: ANY) :: ANY", "The largest value."),
    ("min", "Aggregating", "min(input :: ANY) :: ANY", "The smallest value."),
    ("percentileCont", "Aggregating", "percentileCont(input :: FLOAT, percentile :: FLOAT) :: FLOAT", "The percentile, interpolated between values."),
    ("percentileDisc", "Aggregating", "percentileDisc(input :: INTEGER | FLOAT, percentile :: FLOAT) :: INTEGER | FLOAT", "The percentile, as the nearest value."),
    ("stDev", "Aggregating", "stDev(input :: FLOAT) :: FLOAT", "The standard deviation of a sample."),
    ("stDevP", "Aggregating", "stDevP(input :: FLOAT) :: FLOAT", "The standard deviation of a whole population."),
    ("sum", "Aggregating", "sum(input :: INTEGER | FLOAT | DURATION) :: INTEGER | FLOAT | DURATION", "The sum of numbers or durations."),
    // ---- graph ----
    ("degree", "Graph", "degree(node :: NODE) :: INTEGER", "The number of relationships of a node (graphbuff)."),
    ("elementId", "Graph", "elementId(input :: NODE | RELATIONSHIP) :: STRING", "The element id of a node or relationship."),
    ("endNode", "Graph", "endNode(input :: RELATIONSHIP) :: NODE", "The end node of a relationship."),
    ("id", "Graph", "id(input :: NODE | RELATIONSHIP) :: INTEGER", "The internal id of a node or relationship."),
    ("inDegree", "Graph", "inDegree(node :: NODE) :: INTEGER", "The number of incoming relationships of a node (graphbuff)."),
    ("keys", "Graph", "keys(input :: NODE | RELATIONSHIP | MAP) :: LIST<STRING>", "The property names of a node or relationship, or the keys of a map."),
    ("labels", "Graph", "labels(input :: NODE) :: LIST<STRING>", "The labels of a node."),
    ("nodes", "Graph", "nodes(input :: PATH) :: LIST<NODE>", "The nodes of a path."),
    ("outDegree", "Graph", "outDegree(node :: NODE) :: INTEGER", "The number of outgoing relationships of a node (graphbuff)."),
    ("properties", "Graph", "properties(input :: NODE | RELATIONSHIP | MAP) :: MAP", "The properties of a node or relationship, as a map."),
    ("relationships", "Graph", "relationships(input :: PATH) :: LIST<RELATIONSHIP>", "The relationships of a path."),
    ("rels", "Graph", "rels(input :: PATH) :: LIST<RELATIONSHIP>", "The relationships of a path (as relationships())."),
    ("startNode", "Graph", "startNode(input :: RELATIONSHIP) :: NODE", "The start node of a relationship."),
    ("type", "Graph", "type(input :: RELATIONSHIP) :: STRING", "The type of a relationship."),
    // ---- lists ----
    ("head", "List", "head(list :: LIST<ANY>) :: ANY", "The first element of a list."),
    ("last", "List", "last(list :: LIST<ANY>) :: ANY", "The last element of a list."),
    ("length", "Scalar", "length(input :: PATH | STRING | LIST<ANY>) :: INTEGER", "The number of relationships of a path."),
    ("range", "List", "range(start :: INTEGER, end :: INTEGER, step = 1 :: INTEGER) :: LIST<INTEGER>", "The integers from start to end (inclusive), by step."),
    ("reverse", "List", "reverse(input :: STRING | LIST<ANY>) :: STRING | LIST<ANY>", "A string or list in reverse order."),
    ("size", "Scalar", "size(input :: STRING | LIST<ANY>) :: INTEGER", "The number of characters of a string or elements of a list."),
    ("tail", "List", "tail(list :: LIST<ANY>) :: LIST<ANY>", "A list without its first element."),
    ("toBooleanList", "List", "toBooleanList(input :: LIST<ANY>) :: LIST<BOOLEAN>", "A list with each element converted with toBooleanOrNull()."),
    ("toFloatList", "List", "toFloatList(input :: LIST<ANY>) :: LIST<FLOAT>", "A list with each element converted with toFloatOrNull()."),
    ("toIntegerList", "List", "toIntegerList(input :: LIST<ANY>) :: LIST<INTEGER>", "A list with each element converted with toIntegerOrNull()."),
    ("toStringList", "List", "toStringList(input :: LIST<ANY>) :: LIST<STRING>", "A list with each element converted with toStringOrNull()."),
    // ---- scalar and conversion ----
    ("coalesce", "Scalar", "coalesce(input :: ANY, ...) :: ANY", "The first argument that is not null."),
    ("isEmpty", "Predicate", "isEmpty(input :: STRING | LIST<ANY> | MAP) :: BOOLEAN", "Whether a string, list or map is empty."),
    ("isNaN", "Predicate", "isNaN(input :: INTEGER | FLOAT) :: BOOLEAN", "Whether a number is NaN."),
    ("nullIf", "Scalar", "nullIf(v1 :: ANY, v2 :: ANY) :: ANY", "Null when the two values are equal, else the first."),
    ("randomUUID", "Scalar", "randomUUID() :: STRING", "A random UUID."),
    ("timestamp", "Scalar", "timestamp() :: INTEGER", "Milliseconds since 1970-01-01 UTC."),
    ("toBoolean", "Scalar", "toBoolean(input :: STRING | INTEGER | BOOLEAN) :: BOOLEAN", "A value converted to a boolean; an error for other types."),
    ("toBooleanOrNull", "Scalar", "toBooleanOrNull(input :: ANY) :: BOOLEAN", "A value converted to a boolean, or null."),
    ("toFloat", "Scalar", "toFloat(input :: STRING | INTEGER | FLOAT) :: FLOAT", "A value converted to a float; an error for other types."),
    ("toFloatOrNull", "Scalar", "toFloatOrNull(input :: ANY) :: FLOAT", "A value converted to a float, or null."),
    ("toInteger", "Scalar", "toInteger(input :: STRING | INTEGER | FLOAT | BOOLEAN) :: INTEGER", "A value converted to an integer; an error for other types."),
    ("toIntegerOrNull", "Scalar", "toIntegerOrNull(input :: ANY) :: INTEGER", "A value converted to an integer, or null."),
    ("valueType", "Scalar", "valueType(input :: ANY) :: STRING", "The type of a value, as a string such as \"INTEGER NOT NULL\"."),
    // ---- numbers ----
    ("abs", "Numeric", "abs(input :: INTEGER | FLOAT) :: INTEGER | FLOAT", "The absolute value."),
    ("ceil", "Numeric", "ceil(input :: FLOAT) :: FLOAT", "The smallest integer not less than the number."),
    ("e", "Logarithmic", "e() :: FLOAT", "Euler's number."),
    ("exp", "Logarithmic", "exp(input :: FLOAT) :: FLOAT", "e to the power of the number."),
    ("floor", "Numeric", "floor(input :: FLOAT) :: FLOAT", "The largest integer not greater than the number."),
    ("log", "Logarithmic", "log(input :: FLOAT) :: FLOAT", "The natural logarithm."),
    ("log10", "Logarithmic", "log10(input :: FLOAT) :: FLOAT", "The base-10 logarithm."),
    ("rand", "Numeric", "rand() :: FLOAT", "A random number in [0, 1)."),
    ("round", "Numeric", "round(value :: FLOAT, precision = 0 :: INTEGER, mode = 'HALF_UP' :: STRING) :: FLOAT", "The number rounded, to a precision and with a rounding mode."),
    ("sign", "Numeric", "sign(input :: INTEGER | FLOAT) :: INTEGER", "-1, 0 or 1."),
    ("sqrt", "Logarithmic", "sqrt(input :: FLOAT) :: FLOAT", "The square root."),
    // ---- trigonometry ----
    ("acos", "Trigonometric", "acos(input :: FLOAT) :: FLOAT", "The arccosine, in radians."),
    ("asin", "Trigonometric", "asin(input :: FLOAT) :: FLOAT", "The arcsine, in radians."),
    ("atan", "Trigonometric", "atan(input :: FLOAT) :: FLOAT", "The arctangent, in radians."),
    ("atan2", "Trigonometric", "atan2(y :: FLOAT, x :: FLOAT) :: FLOAT", "The arctangent of y / x, in radians."),
    ("cos", "Trigonometric", "cos(input :: FLOAT) :: FLOAT", "The cosine of an angle in radians."),
    ("cot", "Trigonometric", "cot(input :: FLOAT) :: FLOAT", "The cotangent of an angle in radians."),
    ("degrees", "Trigonometric", "degrees(input :: FLOAT) :: FLOAT", "Radians converted to degrees."),
    ("haversin", "Trigonometric", "haversin(input :: FLOAT) :: FLOAT", "Half the versine of an angle in radians."),
    ("pi", "Trigonometric", "pi() :: FLOAT", "The number pi."),
    ("radians", "Trigonometric", "radians(input :: FLOAT) :: FLOAT", "Degrees converted to radians."),
    ("sin", "Trigonometric", "sin(input :: FLOAT) :: FLOAT", "The sine of an angle in radians."),
    ("tan", "Trigonometric", "tan(input :: FLOAT) :: FLOAT", "The tangent of an angle in radians."),
    // ---- strings ----
    ("btrim", "String", "btrim(original :: STRING, trimCharacterString = ' ' :: STRING) :: STRING", "The string without leading and trailing characters."),
    ("char_length", "Scalar", "char_length(input :: STRING) :: INTEGER", "The number of characters of a string."),
    ("character_length", "Scalar", "character_length(input :: STRING) :: INTEGER", "The number of characters of a string."),
    ("left", "String", "left(original :: STRING, length :: INTEGER) :: STRING", "The first characters of a string."),
    ("lower", "String", "lower(input :: STRING) :: STRING", "The string in lower case (as toLower())."),
    ("ltrim", "String", "ltrim(input :: STRING, trimCharacterString = ' ' :: STRING) :: STRING", "The string without leading characters."),
    ("normalize", "String", "normalize(input :: STRING, normalForm = NFC :: [NFC, NFD, NFKC, NFKD]) :: STRING", "The string in a Unicode normal form."),
    ("replace", "String", "replace(original :: STRING, search :: STRING, replace :: STRING) :: STRING", "The string with every occurrence of search replaced."),
    ("right", "String", "right(original :: STRING, length :: INTEGER) :: STRING", "The last characters of a string."),
    ("rtrim", "String", "rtrim(input :: STRING, trimCharacterString = ' ' :: STRING) :: STRING", "The string without trailing characters."),
    ("split", "String", "split(original :: STRING, splitDelimiter :: STRING | LIST<STRING>) :: LIST<STRING>", "The parts of a string between delimiters."),
    ("substring", "String", "substring(original :: STRING, start :: INTEGER, length = null :: INTEGER) :: STRING", "Part of a string, from a 0-based position."),
    ("toLower", "String", "toLower(input :: STRING) :: STRING", "The string in lower case."),
    ("toString", "String", "toString(input :: ANY) :: STRING", "A value converted to a string; an error for lists, maps and graph elements."),
    ("toStringOrNull", "String", "toStringOrNull(input :: ANY) :: STRING", "A value converted to a string, or null."),
    ("toUpper", "String", "toUpper(input :: STRING) :: STRING", "The string in upper case."),
    ("trim", "String", "trim(input :: STRING) :: STRING", "The string without leading and trailing spaces."),
    ("upper", "String", "upper(input :: STRING) :: STRING", "The string in upper case (as toUpper())."),
    // ---- temporal ----
    ("date", "Temporal", "date(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE", "A date: today, or from a string or map."),
    ("date.realtime", "Temporal", "date.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE", "Today, by the real-time clock."),
    ("date.statement", "Temporal", "date.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE", "Today, by the statement clock."),
    ("date.transaction", "Temporal", "date.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: DATE", "Today, by the transaction clock."),
    ("date.truncate", "Temporal", "date.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: DATE", "A date truncated to a unit."),
    ("datetime", "Temporal", "datetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME", "A zoned date-time: now, or from a string or map."),
    ("datetime.fromEpoch", "Temporal", "datetime.fromEpoch(seconds :: INTEGER | FLOAT, nanoseconds :: INTEGER | FLOAT) :: ZONED DATETIME", "The UTC date-time of seconds since 1970."),
    ("datetime.fromEpochMillis", "Temporal", "datetime.fromEpochMillis(milliseconds :: INTEGER | FLOAT) :: ZONED DATETIME", "The UTC date-time of milliseconds since 1970."),
    ("datetime.realtime", "Temporal", "datetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME", "Now, by the real-time clock."),
    ("datetime.statement", "Temporal", "datetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME", "Now, by the statement clock."),
    ("datetime.transaction", "Temporal", "datetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED DATETIME", "Now, by the transaction clock."),
    ("datetime.truncate", "Temporal", "datetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED DATETIME", "A date-time truncated to a unit."),
    ("duration", "Temporal", "duration(input :: ANY) :: DURATION", "A duration, from a string or map."),
    ("duration.between", "Temporal", "duration.between(from :: ANY, to :: ANY) :: DURATION", "The duration between two temporal values."),
    ("duration.inDays", "Temporal", "duration.inDays(from :: ANY, to :: ANY) :: DURATION", "The duration between two temporal values, in whole days."),
    ("duration.inMonths", "Temporal", "duration.inMonths(from :: ANY, to :: ANY) :: DURATION", "The duration between two temporal values, in whole months."),
    ("duration.inSeconds", "Temporal", "duration.inSeconds(from :: ANY, to :: ANY) :: DURATION", "The duration between two temporal values, in seconds."),
    ("localdatetime", "Temporal", "localdatetime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME", "A local date-time: now, or from a string or map."),
    ("localdatetime.realtime", "Temporal", "localdatetime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME", "Now, by the real-time clock."),
    ("localdatetime.statement", "Temporal", "localdatetime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME", "Now, by the statement clock."),
    ("localdatetime.transaction", "Temporal", "localdatetime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL DATETIME", "Now, by the transaction clock."),
    ("localdatetime.truncate", "Temporal", "localdatetime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL DATETIME", "A local date-time truncated to a unit."),
    ("localtime", "Temporal", "localtime(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME", "A local time: now, or from a string or map."),
    ("localtime.realtime", "Temporal", "localtime.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME", "Now, by the real-time clock."),
    ("localtime.statement", "Temporal", "localtime.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME", "Now, by the statement clock."),
    ("localtime.transaction", "Temporal", "localtime.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: LOCAL TIME", "Now, by the transaction clock."),
    ("localtime.truncate", "Temporal", "localtime.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: LOCAL TIME", "A local time truncated to a unit."),
    ("time", "Temporal", "time(input = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME", "A zoned time: now, or from a string or map."),
    ("time.realtime", "Temporal", "time.realtime(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME", "Now, by the real-time clock."),
    ("time.statement", "Temporal", "time.statement(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME", "Now, by the statement clock."),
    ("time.transaction", "Temporal", "time.transaction(timezone = DEFAULT_TEMPORAL_ARGUMENT :: ANY) :: ZONED TIME", "Now, by the transaction clock."),
    ("time.truncate", "Temporal", "time.truncate(unit :: STRING, input = DEFAULT_TEMPORAL_ARGUMENT :: ANY, fields = null :: MAP) :: ZONED TIME", "A time truncated to a unit."),
    // ---- spatial ----
    ("point", "Spatial", "point(input :: MAP) :: POINT", "A point, from x/y[/z] or longitude/latitude[/height] and an optional crs or srid."),
    ("point.distance", "Spatial", "point.distance(from :: POINT, to :: POINT) :: FLOAT", "The distance between two points of the same CRS (meters for WGS-84)."),
    ("point.withinBBox", "Spatial", "point.withinBBox(point :: POINT, lowerLeft :: POINT, upperRight :: POINT) :: BOOLEAN", "Whether a point lies within a box."),
    // ---- vectors ----
    ("vector.embed", "Vector", "vector.embed(text :: STRING, model = null :: STRING, options = null :: MAP) :: LIST<FLOAT>", "The embedding of a text, by an installed model (the default one if none is named)."),
    ("vector.similarity.cosine", "Vector", "vector.similarity.cosine(a :: LIST<INTEGER | FLOAT>, b :: LIST<INTEGER | FLOAT>) :: FLOAT", "The cosine similarity of two vectors, scaled to [0, 1]."),
    ("vector.similarity.euclidean", "Vector", "vector.similarity.euclidean(a :: LIST<INTEGER | FLOAT>, b :: LIST<INTEGER | FLOAT>) :: FLOAT", "The Euclidean similarity of two vectors, 1 / (1 + d²)."),
    // ---- APOC subset ----
    ("apoc.coll.avg", "APOC", "apoc.coll.avg(coll :: LIST<INTEGER | FLOAT>) :: FLOAT", "The average of a list of numbers."),
    ("apoc.coll.contains", "APOC", "apoc.coll.contains(coll :: LIST<ANY>, value :: ANY) :: BOOLEAN", "Whether a list contains a value."),
    ("apoc.coll.flatten", "APOC", "apoc.coll.flatten(coll :: LIST<ANY>, recursive = false :: BOOLEAN) :: LIST<ANY>", "A list of lists, flattened."),
    ("apoc.coll.indexOf", "APOC", "apoc.coll.indexOf(coll :: LIST<ANY>, value :: ANY) :: INTEGER", "The position of a value in a list, or -1."),
    ("apoc.coll.max", "APOC", "apoc.coll.max(values :: LIST<ANY>) :: ANY", "The largest value of a list."),
    ("apoc.coll.min", "APOC", "apoc.coll.min(values :: LIST<ANY>) :: ANY", "The smallest value of a list."),
    ("apoc.coll.sort", "APOC", "apoc.coll.sort(coll :: LIST<ANY>) :: LIST<ANY>", "A list, sorted."),
    ("apoc.coll.sum", "APOC", "apoc.coll.sum(coll :: LIST<INTEGER | FLOAT>) :: FLOAT", "The sum of a list of numbers."),
    ("apoc.coll.toSet", "APOC", "apoc.coll.toSet(coll :: LIST<ANY>) :: LIST<ANY>", "A list without duplicates."),
    ("apoc.map.fromPairs", "APOC", "apoc.map.fromPairs(pairs :: LIST<LIST<ANY>>) :: MAP", "A map from [key, value] pairs."),
    ("apoc.map.merge", "APOC", "apoc.map.merge(map1 :: MAP, map2 :: MAP) :: MAP", "Two maps merged; the second wins."),
    ("apoc.text.join", "APOC", "apoc.text.join(texts :: LIST<STRING>, delimiter :: STRING) :: STRING", "Strings joined with a delimiter."),
    // ---- predicates ----
    ("exists", "Predicate", "exists(input :: ANY) :: BOOLEAN", "Whether a pattern has a match (or a property is not null)."),
];

/// Whether `name` (lower-cased) aggregates.
fn aggregating(name: &str) -> bool {
    graphbuff_cypher::ast::AGGREGATE_FUNCTIONS.contains(&name)
}

/// `SHOW FUNCTIONS` / `dbms.functions()` rows: name, category, signature, description,
/// aggregating, isBuiltIn.
pub(crate) fn function_rows() -> Vec<Vec<Value>> {
    let mut rows: Vec<&Entry> = FUNCTIONS.iter().collect();
    rows.sort_by_key(|(name, ..)| name.to_ascii_lowercase());
    rows.into_iter()
        .map(|&(name, category, signature, description)| {
            vec![
                Value::String(name.into()),
                Value::String(category.into()),
                Value::String(signature.into()),
                Value::String(description.into()),
                Value::Bool(aggregating(&name.to_ascii_lowercase())),
                Value::Bool(true),
            ]
        })
        .collect()
}

/// The columns of [`function_rows`].
pub(crate) const FUNCTION_COLUMNS: [&str; 6] =
    ["name", "category", "signature", "description", "aggregating", "isBuiltIn"];
