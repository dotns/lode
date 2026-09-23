// Makes the Go SDK importable as a module — both in-repo (the examples use a
// `replace` to this directory) and externally (`go get github.com/dotns/lode/sdks`).
// Stdlib only, no dependencies. Copying lode.go, lock_unix.go and lock_other.go
// into your own module works just as well; this go.mod doesn't change that.
module github.com/dotns/lode/sdks

go 1.22
