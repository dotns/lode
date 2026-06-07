#!/bin/sh
# build.sh — produce a versioned `raw` artifact (single binary) of web-rust.
#
# "Building" bakes the version (and optionally a crash-on-startup flag) into the
# binary via build.rs, yielding a distinct artifact per version — the same
# version-parameterised build the e2e uses for install -> update -> rollback.
#
# Usage:  build.sh <version> [outfile] [--bad]
#   build.sh 0.0.1 dist/0.0.1/web-rust            # good v0.0.1
#   build.sh 0.0.2 dist/0.0.2/web-rust            # good v0.0.2
#   build.sh 0.0.3 dist/0.0.3/web-rust --bad      # crashing v0.0.3 (rollback)
#
# The asset filename (`web-rust`) is the selection key (`[update].asset`); `format`
# is `raw` (derived from the extension) and the advisory `entry` defaults to it.
# lode chmod +x's the entry after install. No [runtime] needed — self-contained.
set -eu

ver="${1:?usage: build.sh <version> [outfile] [--bad]}"
out="${2:-dist/$ver/web-rust}"
bad=""
[ "${3:-}" = "--bad" ] && bad=1

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
cd "$here"

outdir="$(dirname -- "$out")"
[ -d "$outdir" ] || mkdir -p "$outdir"

cargo="${CARGO:-cargo}"
# `env` so ${bad:+...} is passed as a real VAR=value assignment (a bare
# ${bad:+BUILD_BAD=1} prefix would be run as a command after expansion).
env BUILD_VERSION="$ver" ${bad:+BUILD_BAD=1} "$cargo" build --release --quiet

cp -f target/release/web-rust "$out"
chmod +x "$out"

echo "built $out (version $ver${bad:+, BAD})"
