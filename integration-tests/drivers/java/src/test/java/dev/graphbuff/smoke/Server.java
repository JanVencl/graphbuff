package dev.graphbuff.smoke;

import java.io.IOException;
import java.net.HttpURLConnection;
import java.net.ServerSocket;
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.concurrent.TimeUnit;

/** One graphbuff-server process (HTTP + Bolt, token auth) for the whole suite. */
final class Server {
    static final String TOKEN = "smoke-secret";
    private static Server instance;

    final Process process;
    final int boltPort;

    private Server(Process process, int boltPort) {
        this.process = process;
        this.boltPort = boltPort;
    }

    static synchronized Server get() throws Exception {
        if (instance == null) {
            instance = start();
            Runtime.getRuntime().addShutdownHook(new Thread(() -> instance.process.destroy()));
        }
        return instance;
    }

    String boltUri() {
        return "bolt://127.0.0.1:" + boltPort;
    }

    String neo4jUri() {
        return "neo4j://127.0.0.1:" + boltPort;
    }

    private static Path binary() {
        String explicit = System.getenv("GRAPHBUFF_SERVER");
        if (explicit != null && !explicit.isEmpty()) {
            return Path.of(explicit);
        }
        Path root = Path.of(System.getProperty("graphbuff.root", "../../.."));
        String exe = System.getProperty("os.name").toLowerCase().contains("win")
                ? "graphbuff-server.exe" : "graphbuff-server";
        for (String profile : List.of("debug", "release")) {
            Path p = root.resolve("target").resolve(profile).resolve(exe);
            if (Files.exists(p)) {
                return p;
            }
        }
        throw new IllegalStateException(
                "graphbuff-server not built: run `cargo build -p graphbuff-server` or set GRAPHBUFF_SERVER");
    }

    private static int freePort() throws IOException {
        try (ServerSocket s = new ServerSocket(0)) {
            return s.getLocalPort();
        }
    }

    private static Server start() throws Exception {
        int http = freePort();
        int bolt = freePort();
        Process p = new ProcessBuilder(binary().toString(),
                "--bind", "127.0.0.1:" + http,
                "--bolt", "127.0.0.1:" + bolt,
                "--token", TOKEN,
                "--tx-timeout", "5")
                .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.INHERIT)
                .start();
        Instant deadline = Instant.now().plus(Duration.ofSeconds(20));
        while (true) {
            try {
                HttpURLConnection c = (HttpURLConnection) URI.create("http://127.0.0.1:" + http + "/health")
                        .toURL().openConnection();
                c.setConnectTimeout(1000);
                if (c.getResponseCode() == 200) {
                    return new Server(p, bolt);
                }
            } catch (IOException ignored) {
                // not up yet
            }
            if (!p.isAlive() || Instant.now().isAfter(deadline)) {
                p.destroyForcibly().waitFor(5, TimeUnit.SECONDS);
                throw new IllegalStateException("graphbuff-server did not start");
            }
            Thread.sleep(100);
        }
    }
}
