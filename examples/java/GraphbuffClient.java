// A small, dependency-free Java client for graphbuff-server (Java 17+).
//
// Run the demo against a local server:
//
//   cargo run --release -p graphbuff-server
//   java examples/java/GraphbuffClient.java [http://127.0.0.1:7475] [token]
//
// To use it in a project, copy this file and drop `main`. With Jackson or Gson on the
// classpath you can replace the small JSON reader/writer at the bottom.

import java.io.IOException;
import java.net.URI;
import java.net.URLEncoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Collection;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public final class GraphbuffClient {

    /** Columns, rows and update counters of one query. */
    public record Result(List<String> columns, List<List<Object>> rows, Map<String, Object> stats) {
        /** The value in `row` under `column`. */
        public Object get(int row, String column) {
            return rows.get(row).get(columns.indexOf(column));
        }
    }

    /** A query the server rejected: `code` is e.g. SyntaxError, ConstraintError, DatabaseNotFound. */
    public static final class GraphbuffException extends RuntimeException {
        public final int status;
        public final String code;

        GraphbuffException(int status, String code, String message) {
            super(code + ": " + message);
            this.status = status;
            this.code = code;
        }
    }

    private final HttpClient http;
    private final String baseUrl;
    private final String token;

    /** `baseUrl` like "http://127.0.0.1:7475"; `token` may be null. */
    public GraphbuffClient(String baseUrl, String token) {
        this.http = HttpClient.newBuilder()
                .version(HttpClient.Version.HTTP_1_1)
                .connectTimeout(Duration.ofSeconds(5))
                .build();
        this.baseUrl = baseUrl.replaceAll("/+$", "");
        this.token = token;
    }

    /** Runs `cypher` against the default database. */
    public Result query(String cypher, Map<String, ?> params) {
        return post("/query", cypher, params);
    }

    /** Runs `cypher` against `database`. Each call is its own transaction. */
    public Result query(String database, String cypher, Map<String, ?> params) {
        return post("/db/" + URLEncoder.encode(database, StandardCharsets.UTF_8) + "/query", cypher, params);
    }

    private Result post(String path, String cypher, Map<String, ?> params) {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("query", cypher);
        body.put("params", params == null ? Map.of() : params);
        HttpRequest.Builder req = HttpRequest.newBuilder(URI.create(baseUrl + path))
                .timeout(Duration.ofMinutes(5))
                .header("Content-Type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofString(Json.write(body)));
        if (token != null) {
            req.header("Authorization", "Bearer " + token);
        }
        HttpResponse<String> resp;
        try {
            resp = http.send(req.build(), HttpResponse.BodyHandlers.ofString());
        } catch (IOException e) {
            throw new GraphbuffException(0, "ConnectionError", e.getMessage());
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new GraphbuffException(0, "Interrupted", e.getMessage());
        }
        Map<String, Object> json = asMap(Json.read(resp.body()));
        if (resp.statusCode() != 200) {
            Map<String, Object> err = asMap(json.get("error"));
            throw new GraphbuffException(resp.statusCode(), (String) err.get("code"), (String) err.get("message"));
        }
        List<String> columns = new ArrayList<>();
        for (Object c : (List<?>) json.get("columns")) {
            columns.add((String) c);
        }
        List<List<Object>> rows = new ArrayList<>();
        for (Object r : (List<?>) json.get("rows")) {
            rows.add(new ArrayList<>((List<?>) r));
        }
        return new Result(columns, rows, asMap(json.get("stats")));
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> asMap(Object o) {
        return (Map<String, Object>) o;
    }

    // ---- demo ---------------------------------------------------------------------------

    public static void main(String[] args) {
        String url = args.length > 0 ? args[0] : "http://127.0.0.1:7475";
        String token = args.length > 1 ? args[1] : System.getenv("GRAPHBUFF_TOKEN");
        GraphbuffClient db = new GraphbuffClient(url, token);

        db.query("CREATE DATABASE javademo IF NOT EXISTS", null);
        db.query("javademo", "MATCH (n) DETACH DELETE n", null);

        Result created = db.query("javademo",
                "UNWIND $people AS p CREATE (:Person {name: p.name, age: p.age})",
                Map.of("people", List.of(
                        Map.of("name", "Ann", "age", 31),
                        Map.of("name", "Bob", "age", 42),
                        Map.of("name", "Cid", "age", 27))));
        System.out.println("created: " + created.stats().get("nodesCreated") + " nodes");

        db.query("javademo",
                "MATCH (a:Person {name: $a}), (b:Person {name: $b}) CREATE (a)-[:KNOWS {since: $since}]->(b)",
                Map.of("a", "Ann", "b", "Bob", "since", 2020));

        Result r = db.query("javademo",
                "MATCH (p:Person) WHERE p.age > $min RETURN p.name AS name, p.age AS age ORDER BY age",
                Map.of("min", 30));
        for (int i = 0; i < r.rows().size(); i++) {
            String name = (String) r.get(i, "name");
            long age = (Long) r.get(i, "age"); // integers arrive as Long, floats as Double
            System.out.println(name + " is " + age);
        }

        // Nodes and relationships come back as maps with "$type".
        Result k = db.query("javademo", "MATCH (a)-[k:KNOWS]->(b) RETURN a, k, b.name", null);
        Map<String, Object> ann = asMap(k.rows().get(0).get(0));
        Map<String, Object> knows = asMap(k.rows().get(0).get(1));
        System.out.println(ann.get("$type") + " " + ann.get("labels") + " " + ann.get("properties")
                + " -[" + knows.get("type") + " " + knows.get("properties") + "]-> " + k.rows().get(0).get(2));

        try {
            db.query("javademo", "MATCH (n RETURN n", null);
        } catch (GraphbuffException e) {
            System.out.println("rejected: HTTP " + e.status + " " + e.getMessage());
        }
    }

    // ---- minimal JSON -------------------------------------------------------------------

    /** Reads JSON into Map / List / String / Long / Double / Boolean / null, and writes it back. */
    static final class Json {
        private final String s;
        private int pos;

        private Json(String s) {
            this.s = s;
        }

        static Object read(String text) {
            Json j = new Json(text);
            Object v = j.value();
            j.ws();
            if (j.pos != text.length()) {
                throw j.error("trailing data");
            }
            return v;
        }

        private IllegalArgumentException error(String msg) {
            return new IllegalArgumentException("invalid JSON at " + pos + ": " + msg);
        }

        private void ws() {
            while (pos < s.length() && " \t\r\n".indexOf(s.charAt(pos)) >= 0) {
                pos++;
            }
        }

        private Object value() {
            ws();
            if (pos >= s.length()) {
                throw error("unexpected end");
            }
            char c = s.charAt(pos);
            switch (c) {
                case '{': {
                    pos++;
                    Map<String, Object> m = new LinkedHashMap<>();
                    ws();
                    if (s.charAt(pos) == '}') {
                        pos++;
                        return m;
                    }
                    while (true) {
                        ws();
                        String k = string();
                        ws();
                        expect(':');
                        m.put(k, value());
                        ws();
                        if (s.charAt(pos) == ',') {
                            pos++;
                        } else {
                            expect('}');
                            return m;
                        }
                    }
                }
                case '[': {
                    pos++;
                    List<Object> l = new ArrayList<>();
                    ws();
                    if (s.charAt(pos) == ']') {
                        pos++;
                        return l;
                    }
                    while (true) {
                        l.add(value());
                        ws();
                        if (s.charAt(pos) == ',') {
                            pos++;
                        } else {
                            expect(']');
                            return l;
                        }
                    }
                }
                case '"':
                    return string();
                case 't':
                    literal("true");
                    return Boolean.TRUE;
                case 'f':
                    literal("false");
                    return Boolean.FALSE;
                case 'n':
                    literal("null");
                    return null;
                default:
                    return number();
            }
        }

        private void expect(char c) {
            if (pos >= s.length() || s.charAt(pos) != c) {
                throw error("expected '" + c + "'");
            }
            pos++;
        }

        private void literal(String lit) {
            if (!s.startsWith(lit, pos)) {
                throw error("expected " + lit);
            }
            pos += lit.length();
        }

        private Object number() {
            int start = pos;
            boolean integral = true;
            while (pos < s.length() && "+-0123456789.eE".indexOf(s.charAt(pos)) >= 0) {
                if (".eE".indexOf(s.charAt(pos)) >= 0) {
                    integral = false;
                }
                pos++;
            }
            String t = s.substring(start, pos);
            if (t.isEmpty()) {
                throw error("unexpected character");
            }
            if (integral) {
                try {
                    return Long.parseLong(t);
                } catch (NumberFormatException ignored) {
                    // beyond 64 bits
                }
            }
            return Double.parseDouble(t);
        }

        private String string() {
            expect('"');
            StringBuilder b = new StringBuilder();
            while (true) {
                if (pos >= s.length()) {
                    throw error("unterminated string");
                }
                char c = s.charAt(pos++);
                if (c == '"') {
                    return b.toString();
                }
                if (c != '\\') {
                    b.append(c);
                    continue;
                }
                char e = s.charAt(pos++);
                switch (e) {
                    case 'n' -> b.append('\n');
                    case 'r' -> b.append('\r');
                    case 't' -> b.append('\t');
                    case 'b' -> b.append('\b');
                    case 'f' -> b.append('\f');
                    case 'u' -> {
                        b.append((char) Integer.parseInt(s.substring(pos, pos + 4), 16));
                        pos += 4;
                    }
                    default -> b.append(e); // \" \\ \/
                }
            }
        }

        static String write(Object v) {
            StringBuilder b = new StringBuilder();
            write(b, v);
            return b.toString();
        }

        private static void write(StringBuilder b, Object v) {
            if (v == null) {
                b.append("null");
            } else if (v instanceof String str) {
                writeString(b, str);
            } else if (v instanceof Boolean) {
                b.append(v);
            } else if (v instanceof Double || v instanceof Float) {
                double d = ((Number) v).doubleValue();
                if (Double.isNaN(d) || Double.isInfinite(d)) {
                    throw new IllegalArgumentException("JSON cannot carry " + d);
                }
                String t = Double.toString(d); // always has '.' or 'E', so it stays a FLOAT
                b.append(t);
            } else if (v instanceof Number) {
                b.append(((Number) v).longValue());
            } else if (v instanceof Map<?, ?> m) {
                b.append('{');
                boolean first = true;
                for (Map.Entry<?, ?> e : m.entrySet()) {
                    if (!first) {
                        b.append(',');
                    }
                    first = false;
                    writeString(b, String.valueOf(e.getKey()));
                    b.append(':');
                    write(b, e.getValue());
                }
                b.append('}');
            } else if (v instanceof Collection<?> c) {
                b.append('[');
                boolean first = true;
                for (Object x : c) {
                    if (!first) {
                        b.append(',');
                    }
                    first = false;
                    write(b, x);
                }
                b.append(']');
            } else {
                throw new IllegalArgumentException("cannot send " + v.getClass().getName() + " as a parameter");
            }
        }

        private static void writeString(StringBuilder b, String str) {
            b.append('"');
            for (int i = 0; i < str.length(); i++) {
                char c = str.charAt(i);
                switch (c) {
                    case '"' -> b.append("\\\"");
                    case '\\' -> b.append("\\\\");
                    case '\n' -> b.append("\\n");
                    case '\r' -> b.append("\\r");
                    case '\t' -> b.append("\\t");
                    default -> {
                        if (c < 0x20) {
                            b.append(String.format("\\u%04x", (int) c));
                        } else {
                            b.append(c);
                        }
                    }
                }
            }
            b.append('"');
        }
    }
}
