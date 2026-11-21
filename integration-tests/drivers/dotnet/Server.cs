using System.Diagnostics;
using System.Net;
using System.Net.Sockets;
using Xunit;

namespace Graphbuff.Smoke;

/// <summary>One graphbuff-server process (HTTP + Bolt, token auth, in-memory) for the whole suite.</summary>
public sealed class Server : IAsyncLifetime
{
    public const string Token = "smoke-secret";

    private Process? _process;
    private string? _dir;

    public int BoltPort { get; private set; }
    public string BoltUri => $"bolt://127.0.0.1:{BoltPort}";
    public string Neo4jUri => $"neo4j://127.0.0.1:{BoltPort}";

    private static string Binary()
    {
        var explicitPath = Environment.GetEnvironmentVariable("GRAPHBUFF_SERVER");
        if (!string.IsNullOrEmpty(explicitPath)) return explicitPath;
        var exe = OperatingSystem.IsWindows() ? "graphbuff-server.exe" : "graphbuff-server";
        // The engine checkout: the first parent of this directory with target/.
        var root = new DirectoryInfo(AppContext.BaseDirectory);
        while (root != null && !File.Exists(Path.Combine(root.FullName, "Cargo.toml"))) root = root.Parent;
        if (root == null) throw new InvalidOperationException("engine checkout not found; set GRAPHBUFF_SERVER");
        // Whichever of debug and release was built last.
        var built = new[] { "debug", "release" }
            .Select(p => Path.Combine(root.FullName, "target", p, exe))
            .Where(File.Exists)
            .OrderByDescending(File.GetLastWriteTimeUtc)
            .FirstOrDefault();
        return built ?? throw new InvalidOperationException(
            "graphbuff-server not built: run `cargo build -p graphbuff-server` or set GRAPHBUFF_SERVER");
    }

    private static int FreePort()
    {
        var l = new TcpListener(IPAddress.Loopback, 0);
        l.Start();
        var port = ((IPEndPoint)l.LocalEndpoint).Port;
        l.Stop();
        return port;
    }

    public async Task InitializeAsync()
    {
        var http = FreePort();
        BoltPort = FreePort();
        _dir = Directory.CreateTempSubdirectory("graphbuff-config-").FullName;
        var config = Path.Combine(_dir, "graphbuff.toml");
        await File.WriteAllTextAsync(config, string.Join("\n",
            "[server]",
            $"bind = \"127.0.0.1:{http}\"",
            $"bolt = \"127.0.0.1:{BoltPort}\"",
            "[storage]",
            "mode = \"memory\"",
            "[security]",
            $"token = \"{Token}\"",
            "[queries]",
            "transaction_timeout_seconds = 5",
            "[logs]",
            "dir = \"-\"",
            ""));
        var start = new ProcessStartInfo(Binary()) { UseShellExecute = false, RedirectStandardOutput = true };
        start.ArgumentList.Add("--config");
        start.ArgumentList.Add(config);
        foreach (var key in start.Environment.Keys.Where(k => k.StartsWith("GRAPHBUFF_")).ToList())
            start.Environment.Remove(key);
        _process = Process.Start(start) ?? throw new InvalidOperationException("graphbuff-server did not start");
        _process.OutputDataReceived += (_, _) => { };
        _process.BeginOutputReadLine();

        using var client = new HttpClient { Timeout = TimeSpan.FromSeconds(1) };
        var deadline = DateTime.UtcNow.AddSeconds(20);
        while (true)
        {
            try
            {
                var res = await client.GetAsync($"http://127.0.0.1:{http}/health");
                if (res.IsSuccessStatusCode) return;
            }
            catch (Exception e) when (e is HttpRequestException or TaskCanceledException)
            {
                // not up yet
            }
            if (_process.HasExited || DateTime.UtcNow > deadline)
            {
                if (!_process.HasExited) _process.Kill();
                throw new InvalidOperationException("graphbuff-server did not start");
            }
            await Task.Delay(100);
        }
    }

    public async Task DisposeAsync()
    {
        if (_process is { HasExited: false })
        {
            _process.Kill();
            await _process.WaitForExitAsync();
        }
        _process?.Dispose();
        if (_dir != null) Directory.Delete(_dir, recursive: true);
    }
}
