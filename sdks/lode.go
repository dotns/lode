// Package lode is a single-file Go SDK for the `lode` supervisor (github.com/dotns/lode).
// Wraps the state.json contract: read status, request upgrade/restart/rollback,
// report readiness, subscribe to lode's notifications. The SDK only *signals* lode
// (writes target/restart_nonce/ready under state.json.lock); lode does the heavy
// fetch→verify→install→observe. Stdlib only, Unix. Contract: ../docs/integration.md §2.
package lode

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"
)

// Status is the lifecycle status lode reports (kebab-case on the wire).
type Status string

const (
	StatusStarting    Status = "starting"
	StatusRunning     Status = "running"
	StatusHeld        Status = "held"
	StatusUpdating    Status = "updating"
	StatusRollingBack Status = "rolling-back"
	StatusStopping    Status = "stopping"
	StatusStopped     Status = "stopped"
	StatusError       Status = "error"
)

// HistoryEntry is one entry in lode's rollout history.
type HistoryEntry struct {
	Version string `json:"version"`
	At      string `json:"at"`
	Result  string `json:"result"` // "good" | "bad"
}

// State is the parsed state.json. lode writes the top group; the app writes
// Target/RestartNonce/Ready.
type State struct {
	Current          string         `json:"current,omitempty"`
	LastGood         string         `json:"last_good,omitempty"`
	Available        string         `json:"available,omitempty"`
	Channel          string         `json:"channel,omitempty"`
	Status           Status         `json:"status,omitempty"`
	PID              int            `json:"pid,omitempty"`
	LastCheck        string         `json:"last_check,omitempty"`
	LastError        string         `json:"last_error,omitempty"`
	History          []HistoryEntry `json:"history,omitempty"`
	ConfigGeneration uint64         `json:"config_generation,omitempty"`
	Target           string         `json:"target,omitempty"`
	RestartNonce     uint64         `json:"restart_nonce,omitempty"`
	Hold             bool           `json:"hold,omitempty"`
	Ready            string         `json:"ready,omitempty"`
}

// Client is a handle on one lode data directory. FromEnv for the supervised app;
// New(dir, "") for an external tool.
type Client struct {
	dataDir  string
	instance string
}

// New returns a Client for an explicit data dir and instance id. instance may be
// "" when you only issue requests (target / restart) and never report readiness.
func New(dataDir, instance string) *Client { return &Client{dataDir: dataDir, instance: instance} }

// FromEnv builds a Client from the injected env (LODE_DATA_DIR / LODE_INSTANCE).
func FromEnv() (*Client, error) {
	dir := os.Getenv("LODE_DATA_DIR")
	if dir == "" {
		return nil, errors.New("lode: LODE_DATA_DIR not set — run under lode, or use lode.New")
	}
	return &Client{dataDir: dir, instance: os.Getenv("LODE_INSTANCE")}, nil
}

// DataDir reports the data directory this Client targets.
func (c *Client) DataDir() string { return c.dataDir }

// Instance reports this launch's unique id (empty when not supervised).
func (c *Client) Instance() string { return c.instance }

func (c *Client) statePath() string { return filepath.Join(c.dataDir, "state.json") }
func (c *Client) lockPath() string  { return filepath.Join(c.dataDir, "state.json.lock") }

// Read parses state.json. Returns (nil, nil) when absent. Lock-free — atomic
// rename guarantees a whole snapshot.
func (c *Client) Read() (*State, error) {
	b, err := os.ReadFile(c.statePath())
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var s State
	if err := json.Unmarshal(b, &s); err != nil {
		return nil, err
	}
	return &s, nil
}

// Update is the locked RMW primitive: patch mutates the raw object (snake_case
// keys); unknown keys round-trip verbatim (numbers via json.Number, no precision
// loss). The request/readiness helpers below wrap it.
func (c *Client) Update(patch func(map[string]any)) (*State, error) {
	// Best-effort flock(2) on the sibling lock file, matching lode's own RMW lock.
	if lf, err := os.OpenFile(c.lockPath(), os.O_CREATE|os.O_APPEND, 0o644); err == nil {
		defer lf.Close()
		fd := int(lf.Fd())
		if syscall.Flock(fd, syscall.LOCK_EX) == nil {
			defer syscall.Flock(fd, syscall.LOCK_UN)
		}
	}

	m := map[string]any{}
	if b, err := os.ReadFile(c.statePath()); err == nil && len(b) > 0 {
		dec := json.NewDecoder(bytes.NewReader(b))
		dec.UseNumber()
		tmp := map[string]any{}
		if dec.Decode(&tmp) == nil {
			m = tmp // corrupt → keep the empty map (lenient, like lode)
		}
	}

	patch(m)

	out, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return nil, err
	}
	out = append(out, '\n')
	tmp := fmt.Sprintf("%s.%d.tmp", c.statePath(), os.Getpid())
	if err := os.WriteFile(tmp, out, 0o644); err != nil {
		return nil, err
	}
	if err := os.Rename(tmp, c.statePath()); err != nil {
		_ = os.Remove(tmp)
		return nil, err
	}

	var s State
	_ = json.Unmarshal(out, &s)
	return &s, nil
}

// Reboot asks lode to restart your own process — a clean graceful stop (SIGTERM)
// + respawn of the current version. Use to self-recycle (you detected a resource
// leak, or on a periodic schedule), or to apply a lode.toml/[env] edit (the
// Run-phase restart re-reads lode.toml). Bumps restart_nonce; lode acts ~1s later,
// once per bump. Returns the new nonce.
func (c *Client) Reboot() (uint64, error) {
	var next uint64
	_, err := c.Update(func(m map[string]any) {
		next = toUint64(m["restart_nonce"]) + 1
		m["restart_nonce"] = next
	})
	return next, err
}

// ReloadConfig applies a pending lode.toml edit — alias of Reboot (the restart
// re-reads lode.toml). Returns the new nonce.
func (c *Client) ReloadConfig() (uint64, error) { return c.Reboot() }

// RequestUpdate sets target (a version or "latest") to request an up/down-grade.
func (c *Client) RequestUpdate(version string) error {
	if version == "" {
		return errors.New("lode: RequestUpdate needs a non-empty version")
	}
	_, err := c.Update(func(m map[string]any) { m["target"] = version })
	return err
}

// Hold asks lode NOT to (re)start your process (maintenance) → status "held"; a
// running child is left alone. Clear with Release.
func (c *Client) Hold() error {
	_, err := c.Update(func(m map[string]any) { m["hold"] = true })
	return err
}

// Release clears a hold (see Hold) → lode resumes (re)starting your process.
func (c *Client) Release() error {
	_, err := c.Update(func(m map[string]any) { m["hold"] = false })
	return err
}

// Rollback sets target to version, or — when "" — to the recorded last_good.
// Returns the chosen version, or an error if neither exists.
func (c *Client) Rollback(version string) (string, error) {
	chosen := version
	_, err := c.Update(func(m map[string]any) {
		if chosen == "" {
			if lg, ok := m["last_good"].(string); ok {
				chosen = lg
			}
		}
		if chosen != "" {
			m["target"] = chosen
		}
	})
	if err != nil {
		return "", err
	}
	if chosen == "" {
		return "", errors.New("lode: rollback needs a version or a recorded last_good")
	}
	return chosen, nil
}

// MarkReady reports "I can serve" with the bare token. Use unless you opt into
// the phased handshake.
func (c *Client) MarkReady() error { return c.setReady(c.instance) }

// MarkServing reports serving as "{instance}-0" (phased handshake).
func (c *Client) MarkServing() error { return c.setReady(c.instance + "-0") }

// AckPrepared acks "prepared, cut over" as "{instance}-2" (phased handshake).
func (c *Client) AckPrepared() error { return c.setReady(c.instance + "-2") }

// PrepareRequested reports whether lode is prompting THIS instance to prepare
// (ready == "{instance}-1"). Pass a non-nil state to avoid a re-read.
func (c *Client) PrepareRequested(s *State) bool {
	if c.instance == "" {
		return false
	}
	if s == nil {
		var err error
		if s, err = c.Read(); err != nil || s == nil {
			return false
		}
	}
	return s.Ready == c.instance+"-1"
}

func (c *Client) setReady(token string) error {
	if c.instance == "" {
		return errors.New("lode: no LODE_INSTANCE — readiness needs a supervised launch")
	}
	_, err := c.Update(func(m map[string]any) { m["ready"] = token })
	return err
}

// Handlers are the callbacks for Watch — lode's notifications. Each fires on
// change only; any may be nil.
type Handlers struct {
	OnConfigChange  func(generation uint64, state *State)        // config_generation rose (lode.toml edited)
	OnAvailable     func(version string, state *State)           // newer version advertised (policy = check)
	OnStatus        func(status Status, state *State)            // lifecycle status changed
	OnVersionChange func(current, lastGood string, state *State) // an update committed / a rollback landed
	OnHold          func(held bool, state *State)                // the hold flag was set/cleared
	OnError         func(message string, state *State)           // lode recorded a (non-fatal) error
	OnPrepare       func(state *State)                           // staged-update prepare prompt; then AckPrepared
	OnState         func(state *State)                           // every tick, the full snapshot
}

// Watch polls state.json every interval (default 1s), firing h's callbacks on
// change, until stop is closed. Run it in its own goroutine.
func (c *Client) Watch(stop <-chan struct{}, interval time.Duration, h Handlers) {
	if interval <= 0 {
		interval = time.Second
	}
	var gen uint64
	var status Status
	var available, lastError, current, lastGood string
	var hold bool
	if s, _ := c.Read(); s != nil {
		gen, status, available = s.ConfigGeneration, s.Status, s.Available
		lastError, current, lastGood, hold = s.LastError, s.Current, s.LastGood, s.Hold
	}
	prompted := false
	t := time.NewTicker(interval)
	defer t.Stop()
	for {
		select {
		case <-stop:
			return
		case <-t.C:
			s, _ := c.Read()
			if s == nil {
				continue
			}
			if h.OnState != nil {
				h.OnState(s)
			}
			if s.ConfigGeneration > gen {
				gen = s.ConfigGeneration
				if h.OnConfigChange != nil {
					h.OnConfigChange(gen, s)
				}
			}
			if s.Available != available {
				available = s.Available
				if s.Available != "" && h.OnAvailable != nil {
					h.OnAvailable(s.Available, s)
				}
			}
			if s.Status != status {
				status = s.Status
				if s.Status != "" && h.OnStatus != nil {
					h.OnStatus(s.Status, s)
				}
			}
			if s.Current != current || s.LastGood != lastGood {
				current, lastGood = s.Current, s.LastGood
				if h.OnVersionChange != nil {
					h.OnVersionChange(s.Current, s.LastGood, s)
				}
			}
			if s.Hold != hold {
				hold = s.Hold
				if h.OnHold != nil {
					h.OnHold(s.Hold, s)
				}
			}
			if s.LastError != lastError {
				lastError = s.LastError
				if s.LastError != "" && h.OnError != nil {
					h.OnError(s.LastError, s)
				}
			}
			if c.instance != "" && s.Ready == c.instance+"-1" {
				if !prompted {
					prompted = true
					if h.OnPrepare != nil {
						h.OnPrepare(s)
					}
				}
			} else {
				prompted = false
			}
		}
	}
}

// IsSupervised reports whether this process is supervised by lode (LODE_DATA_DIR set).
func IsSupervised() bool { return os.Getenv("LODE_DATA_DIR") != "" }

// ActiveVersion is the version lode launched (LODE_ACTIVE_VERSION).
func ActiveVersion() string { return os.Getenv("LODE_ACTIVE_VERSION") }

// InstanceID is this launch's unique id (LODE_INSTANCE, "{pid}-{nanoid}").
func InstanceID() string { return os.Getenv("LODE_INSTANCE") }

// Readiness is the readiness mode in force ("none" | "state").
func Readiness() string { return os.Getenv("LODE_READINESS") }

// OnTerminate registers the graceful-stop handler: on SIGTERM/SIGINT it runs
// handler then exits 0.
func OnTerminate(handler func()) {
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-ch
		if handler != nil {
			handler()
		}
		os.Exit(0)
	}()
}

// toUint64 coerces a json.Number / float64 / int(64) / uint64 to uint64.
func toUint64(v any) uint64 {
	switch n := v.(type) {
	case json.Number:
		if i, err := n.Int64(); err == nil && i >= 0 {
			return uint64(i)
		}
	case float64:
		if n >= 0 {
			return uint64(n)
		}
	case int:
		if n >= 0 {
			return uint64(n)
		}
	case int64:
		if n >= 0 {
			return uint64(n)
		}
	case uint64:
		return n
	}
	return 0
}
