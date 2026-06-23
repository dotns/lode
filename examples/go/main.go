// Package main is a lode demo app (Go). See ../README.md.
//
// It conforms to the lode app contract via the SDK (github.com/dotns/lode/sdks,
// a local replace to ../../sdks/lode.go) and shows the three things an app does
// under lode:
//
//  1. START   — bind $PORT and serve; lode runs this binary as its child.
//  2. READ    — read the env lode injects (lode.ActiveVersion / LODE_DATA_DIR /
//     lode.InstanceID) plus passthrough host env (PORT, operator [env]).
//  3. UPGRADE — a) PASSIVE: MarkReady() + lode.OnTerminate(), so lode's update/
//     rollback is seamless;
//     b) ACTIVE:  the endpoints below call RequestUpdate / Reboot /
//     Hold / Release.
//
// Standalone (no lode): LODE_DATA_DIR is unset, so lode.FromEnv() errors and the
// request endpoints reply 503 — you still get a working server.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"os"
	"time"

	lode "github.com/dotns/lode/sdks"
)

// buildVersion is the fallback baked at build time:
//
//	go build -ldflags "-X main.buildVersion=1.2.3" -o demo-go .
//
// At runtime lode's LODE_ACTIVE_VERSION wins, so /version always matches what
// lode actually installed.
var buildVersion = "0.0.0-dev"

func version() string {
	if v := lode.ActiveVersion(); v != "" {
		return v
	}
	return buildVersion
}

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func logf(format string, a ...any) { fmt.Printf("[demo-go] "+format+"\n", a...) }

func main() {
	// `lode version` passthrough when the operator sets exec = "./demo-go".
	if len(os.Args) > 1 {
		switch os.Args[1] {
		case "version", "--version", "-v":
			fmt.Println(version())
			return
		}
	}

	port := env("PORT", "8080")
	addr := ":" + port

	// The SDK handle — nil when run standalone (LODE_DATA_DIR unset).
	client, _ := lode.FromEnv()

	// ask runs an SDK request, or replies 503 when not supervised by lode.
	ask := func(w http.ResponseWriter, fn func(*lode.Client) error, ok string) {
		if client == nil {
			http.Error(w, "not running under lode (LODE_DATA_DIR unset)", http.StatusServiceUnavailable)
			return
		}
		if err := fn(client); err != nil {
			http.Error(w, err.Error(), http.StatusServiceUnavailable)
			return
		}
		fmt.Fprintln(w, ok)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) { fmt.Fprintln(w, "ok") })
	mux.HandleFunc("/version", func(w http.ResponseWriter, _ *http.Request) { fmt.Fprintln(w, version()) })
	// READ: surface the env lode injected + passthrough host/operator env.
	mux.HandleFunc("/env", func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"version":  version(),         // LODE_ACTIVE_VERSION or baked
			"instance": lode.InstanceID(), // unique id per launch
			"dataDir":  os.Getenv("LODE_DATA_DIR"),
			"port":     port,                      // host env passthrough
			"greeting": os.Getenv("APP_GREETING"), // operator [env] / host -e
		})
	})
	// UPGRADE (active) + maintenance.
	mux.HandleFunc("/upgrade", func(w http.ResponseWriter, _ *http.Request) {
		ask(w, func(c *lode.Client) error { return c.RequestUpdate("latest") }, "requested update to latest")
	})
	mux.HandleFunc("/restart", func(w http.ResponseWriter, _ *http.Request) {
		ask(w, func(c *lode.Client) error { _, err := c.Reboot(); return err }, "requested restart")
	})
	mux.HandleFunc("/hold", func(w http.ResponseWriter, _ *http.Request) {
		ask(w, func(c *lode.Client) error { return c.Hold() }, "held (lode will not (re)start the app)")
	})
	mux.HandleFunc("/release", func(w http.ResponseWriter, _ *http.Request) {
		ask(w, func(c *lode.Client) error { return c.Release() }, "released")
	})

	srv := &http.Server{Handler: mux}

	// START: bind first so readiness is announced only once we can serve.
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		fmt.Fprintf(os.Stderr, "[demo-go] bind %s: %v\n", addr, err)
		os.Exit(1)
	}
	logf("starting version=%s pid=%d instance=%s data_dir=%s addr=%s",
		version(), os.Getpid(), env("LODE_INSTANCE", "none"), env("LODE_DATA_DIR", "unset"), addr)

	// UPGRADE (passive): graceful stop — on SIGTERM/SIGINT drain and exit(0) within
	// supervise.stop_timeout (the SDK calls os.Exit(0) after the handler returns).
	lode.OnTerminate(func() {
		logf("shutting down")
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(ctx)
	})

	// UPGRADE (passive): announce readiness so lode (readiness="state") commits us.
	if client != nil {
		if err := client.MarkReady(); err != nil {
			logf("readiness skipped: %v", err)
		} else {
			logf("ready: state.ready=%s", lode.InstanceID())
		}
	} else {
		logf("readiness skipped (standalone)")
	}

	if err := srv.Serve(ln); err != nil && err != http.ErrServerClosed {
		fmt.Fprintf(os.Stderr, "[demo-go] serve: %v\n", err)
		os.Exit(1)
	}
	logf("cleanup done, exiting 0")
}
