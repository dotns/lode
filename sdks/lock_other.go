//go:build !unix

package lode

// lockState is a no-op where flock(2) does not exist. lode itself is Unix-only, so a process on
// such a platform is never supervised and never shares state.json with it; the package compiles so
// cross-platform apps can vendor this SDK and branch on IsSupervised.
func lockState(string) func() { return func() {} }
