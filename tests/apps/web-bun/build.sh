#!/bin/sh
# build.sh — produce a versioned `raw` script artifact of web-bun.
#
# "Building" bakes the version (and optionally a crash-on-startup flag) into a
# copy of app.ts, yielding a distinct artifact per version — the same
# version-parameterised build the e2e uses for install -> update -> rollback.
#
# Usage:  build.sh <version> [outfile] [--bad]
#   build.sh 0.0.1 dist/0.0.1/app.ts            # good v0.0.1
#   build.sh 0.0.2 dist/0.0.2/app.ts            # good v0.0.2
#   build.sh 0.0.3 dist/0.0.3/app.ts --bad      # crashing v0.0.3 (rollback)
#
# The asset filename (`app.ts`) is the selection key (`[update].asset`); `format`
# is `raw` (derived from the extension) and the file lands under that filename in
# the version dir.
# Runs under lode via `[runtime] bun` (lode downloads bun if absent).
#
# Optional: pass --bundle to emit a single self-contained .js via `bun build`
# (the artifact lands as app.js); run it with run = "bun run app.js".
set -eu

ver="${1:?usage: build.sh <version> [outfile] [--bad] [--bundle]}"
out="${2:-dist/$ver/app.ts}"

bad=""
bundle=""
shift 2 2>/dev/null || shift $#
for f in "$@"; do
  [ "$f" = "--bad" ] && bad=1
  [ "$f" = "--bundle" ] && bundle=1
done

here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
src="$here/app.ts"

outdir="$(dirname -- "$out")"
[ -d "$outdir" ] || mkdir -p "$outdir"

# Bake the version (and BAD flag): rewrite the two single BUILD_* lines.
sed \
  -e 's/^const BUILD_VERSION = .*/const BUILD_VERSION = "'"$ver"'";/' \
  -e "s/^const BUILD_BAD = .*/const BUILD_BAD = \"${bad:-0}\";/" \
  "$src" > "$out"

if [ -n "$bundle" ]; then
  js="${out%.ts}.js"
  "${BUN:-bun}" build "$out" --target bun --outfile "$js" >&2
  echo "built $js (version $ver${bad:+, BAD}, bundled)"
else
  echo "built $out (version $ver${bad:+, BAD})"
fi
