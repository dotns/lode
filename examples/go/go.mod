module lode-demo-go

go 1.22

// Uses the lode Go SDK (../../sdks/lode.go) via a local replace. The SDK is
// stdlib-only, so this still builds offline into a self-contained static binary
// (the artifact lode installs).
require github.com/dotns/lode/sdks v0.0.0

replace github.com/dotns/lode/sdks => ../../sdks
