//go:build unix

package lode

import (
	"os"
	"syscall"
)

// lockState takes lode's advisory lock on the sibling lock file and returns the unlock function.
// Best effort: a lock that cannot be taken still runs the read-modify-write, as lode tolerates.
func lockState(path string) func() {
	lf, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND, 0o644)
	if err != nil {
		return func() {}
	}
	if err := syscall.Flock(int(lf.Fd()), syscall.LOCK_EX); err != nil {
		_ = lf.Close()
		return func() {}
	}
	return func() {
		_ = syscall.Flock(int(lf.Fd()), syscall.LOCK_UN)
		_ = lf.Close()
	}
}
