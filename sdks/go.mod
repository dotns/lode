// Makes the single-file Go SDK importable as a module — both in-repo (the
// examples use a `replace` to this directory) and externally once tagged
// (`go get github.com/dotns/lode/sdks`). Stdlib only, no dependencies. Copying
// lode.go into your own module works just as well; this go.mod doesn't change that.
module github.com/dotns/lode/sdks

go 1.22
