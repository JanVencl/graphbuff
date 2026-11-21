package smoke

import (
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

// token is the server's admin token; the suite logs in with it.
const token = "smoke-secret"

// server is one graphbuff-server process (HTTP + Bolt, token auth, in-memory).
type server struct {
	cmd      *exec.Cmd
	dir      string
	boltPort int
}

func (s *server) boltURI() string  { return fmt.Sprintf("bolt://127.0.0.1:%d", s.boltPort) }
func (s *server) neo4jURI() string { return fmt.Sprintf("neo4j://127.0.0.1:%d", s.boltPort) }

// binary is GRAPHBUFF_SERVER, else whichever of target/debug and target/release of the
// engine checkout (the first parent directory with Cargo.toml) was built last.
func binary() (string, error) {
	if p := os.Getenv("GRAPHBUFF_SERVER"); p != "" {
		return p, nil
	}
	exe := "graphbuff-server"
	if runtime.GOOS == "windows" {
		exe += ".exe"
	}
	dir, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "Cargo.toml")); err == nil {
			break
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("engine checkout not found; set GRAPHBUFF_SERVER")
		}
		dir = parent
	}
	best, bestTime := "", time.Time{}
	for _, profile := range []string{"debug", "release"} {
		p := filepath.Join(dir, "target", profile, exe)
		if st, err := os.Stat(p); err == nil && st.ModTime().After(bestTime) {
			best, bestTime = p, st.ModTime()
		}
	}
	if best == "" {
		return "", fmt.Errorf("graphbuff-server not built: run `cargo build -p graphbuff-server` or set GRAPHBUFF_SERVER")
	}
	return best, nil
}

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

func startServer() (*server, error) {
	bin, err := binary()
	if err != nil {
		return nil, err
	}
	httpPort, err := freePort()
	if err != nil {
		return nil, err
	}
	boltPort, err := freePort()
	if err != nil {
		return nil, err
	}
	dir, err := os.MkdirTemp("", "graphbuff-config-")
	if err != nil {
		return nil, err
	}
	config := filepath.Join(dir, "graphbuff.toml")
	content := strings.Join([]string{
		"[server]",
		fmt.Sprintf("bind = \"127.0.0.1:%d\"", httpPort),
		fmt.Sprintf("bolt = \"127.0.0.1:%d\"", boltPort),
		"[storage]",
		"mode = \"memory\"",
		"[security]",
		fmt.Sprintf("token = \"%s\"", token),
		"[queries]",
		"transaction_timeout_seconds = 5",
		"[logs]",
		"dir = \"-\"",
		"",
	}, "\n")
	if err := os.WriteFile(config, []byte(content), 0o600); err != nil {
		return nil, err
	}
	cmd := exec.Command(bin, "--config", config)
	for _, kv := range os.Environ() {
		if !strings.HasPrefix(kv, "GRAPHBUFF_") {
			cmd.Env = append(cmd.Env, kv)
		}
	}
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	exited := make(chan struct{})
	go func() { _ = cmd.Wait(); close(exited) }()

	client := http.Client{Timeout: time.Second}
	deadline := time.Now().Add(20 * time.Second)
	for {
		res, err := client.Get(fmt.Sprintf("http://127.0.0.1:%d/health", httpPort))
		if err == nil {
			res.Body.Close()
			if res.StatusCode == http.StatusOK {
				return &server{cmd: cmd, dir: dir, boltPort: boltPort}, nil
			}
		}
		select {
		case <-exited:
			return nil, fmt.Errorf("graphbuff-server exited before it was up")
		default:
		}
		if time.Now().After(deadline) {
			_ = cmd.Process.Kill()
			return nil, fmt.Errorf("graphbuff-server did not start")
		}
		time.Sleep(100 * time.Millisecond)
	}
}

func (s *server) stop() {
	_ = s.cmd.Process.Kill()
	_ = os.RemoveAll(s.dir)
}
