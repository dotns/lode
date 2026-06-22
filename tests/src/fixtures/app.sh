#!/bin/sh
# lode e2e test app — a versioned, mode-parameterised POSIX-sh artifact driven by
# the real lode binary (implements the readiness/stop contract); the BUILD_* lines
# below are rewritten per build by the TS app builder (tests/src/helpers/app.ts).
#
# This is the APPLICATION ARTIFACT under test, NOT a test runner — every bit of
# e2e orchestration is bun+TypeScript. Pure POSIX sh, no jq/python/etc.
#
# lode injects (design §10): LODE_ACTIVE_VERSION, LODE_DATA_DIR, LODE_INSTANCE,
# LODE_READINESS. errexit is intentionally NOT used (a trap-driven supervise loop
# must survive the signal-interrupted `wait`); `set -u` catches unset-var typos.
set -u

# --- baked by the builder (keep these six lines simple: `NAME="value"`) ---
BUILD_VERSION="0.0.0-dev"
BUILD_MODE="service"      # service | exit | update-on-exit
BUILD_EXIT_CODE="0"       # process exit code for BUILD_MODE=exit
BUILD_TARGET=""           # version to request for BUILD_MODE=update-on-exit
BUILD_GATE="0"            # service + readiness=state: defer serving ready (-0) until $LODE_DATA_DIR/ready_ok exists
BUILD_PREPARE_GATE="0"    # service + readiness=state: defer the prepare ack (-2) until $LODE_DATA_DIR/prepare_ok exists

# When run under lode, LODE_ACTIVE_VERSION (injected) wins, so the self-reported
# version always matches what lode installed.
VERSION="${LODE_ACTIVE_VERSION:-$BUILD_VERSION}"
log() { printf '[app] %s\n' "$*"; }

# --- CLI passthrough subcommands (the `lode <args>` exec path) -------------
case "${1:-}" in
  version | --version | -v)
    printf '%s\n' "$VERSION"
    exit 0
    ;;
  print)
    # print <text> <code>: emit <text> on stdout, exit with <code> (default 0).
    # Lets the e2e assert that stdout AND the exit code propagate through exec.
    shift
    printf '%s\n' "${1:-}"
    exit "${2:-0}"
    ;;
esac

# --- graceful-stop contract (lode -> app: SIGTERM) ------------------------
# lode sets status=stopping then SIGTERMs us; we must clean up and exit 0 within
# supervise.stop_timeout or get SIGKILLed. The trap is installed before any loop.
running=1
on_term() {
  running=0
  log "SIGTERM received — cleaning up"
  log "cleanup done, exiting 0"
  exit 0
}
trap on_term TERM INT

# --- atomic state.json field write (preserves lode-owned fields) ----------
# set_state_field <key> <string-value>: replace the key's value if present, else
# insert the key right after the opening brace, else create a minimal object —
# then temp+rename (atomic). String values only; values here (instance ids,
# versions) contain no '/' so the sed delimiter is safe.
set_state_field() {
  [ -n "${LODE_DATA_DIR:-}" ] || return 0
  _k="$1"
  _v="$2"
  _s="$LODE_DATA_DIR/state.json"
  _t="$_s.$_k.$$"
  if [ -f "$_s" ] && grep -q "\"$_k\"" "$_s"; then
    sed 's/"'"$_k"'"[[:space:]]*:[^,}]*/"'"$_k"'": "'"$_v"'"/' "$_s" > "$_t"
  elif [ -f "$_s" ]; then
    sed '1 s/{/{ "'"$_k"'": "'"$_v"'",/' "$_s" > "$_t"
  else
    printf '{\n  "%s": "%s"\n}\n' "$_k" "$_v" > "$_t"
  fi
  mv -f "$_t" "$_s"
}

# Staged-update + readiness handshake (app -> lode), only when readiness=state. The
# value is "$LODE_INSTANCE-$phase": "-0" = serving, "-2" = prepared/cut-over-now
# (lode prompts a staged update with "-1"). LODE_INSTANCE is this spawn's unique id.
write_ready() {
  [ "${LODE_READINESS:-none}" = "state" ] || return 0
  set_state_field ready "${LODE_INSTANCE:-}-$1"
  log "ready: wrote state.ready=${LODE_INSTANCE:-}-$1"
}

# Extract the current state.ready value (empty if absent); used to spot lode's "-1"
# staged-update prompt. Pure sed — "ready" appears only as this one field.
read_ready() {
  _s="${LODE_DATA_DIR:-}/state.json"
  [ -f "$_s" ] || return 0
  sed -n 's/.*"ready"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_s" | head -n1
}

# --- non-service modes (exit immediately) ---------------------------------
case "$BUILD_MODE" in
  exit)
    log "starting version=$VERSION pid=$$ instance=${LODE_INSTANCE:-none} mode=exit code=$BUILD_EXIT_CODE"
    exit "$BUILD_EXIT_CODE"
    ;;
  update-on-exit)
    log "starting version=$VERSION pid=$$ instance=${LODE_INSTANCE:-none} mode=update-on-exit target=$BUILD_TARGET"
    set_state_field target "$BUILD_TARGET"
    log "wrote state.target=$BUILD_TARGET; exiting 0"
    exit 0
    ;;
esac

# --- service mode (long-running) ------------------------------------------
log "starting version=$VERSION pid=$$ instance=${LODE_INSTANCE:-none} data_dir=${LODE_DATA_DIR:-unset} marker=${RELOAD_MARKER:-none}"

if [ "$BUILD_GATE" = "1" ]; then
  # Announce serving readiness (-0) only once the test drops the gate file — this
  # lets the e2e observe lode WAITING for the readiness handshake post-cut-over.
  while [ "$running" -eq 1 ]; do
    if [ -f "${LODE_DATA_DIR:-}/ready_ok" ]; then
      write_ready 0
      break
    fi
    sleep 0.2 &
    wait "$!" 2>/dev/null || true
  done
else
  write_ready 0
fi

# Supervise loop (background `sleep` + `wait` so the SIGTERM trap fires sub-second).
# Under readiness=state we also answer lode's staged-update prompt: when state.ready
# becomes "$LODE_INSTANCE-1", prepare then ack with "-2" so lode cuts over. With a
# prepare gate, defer the ack until the test drops prepare_ok — letting the e2e
# observe lode WAITING (no cut-over) while we "prepare".
acked_prepare=0
while [ "$running" -eq 1 ]; do
  if [ "${LODE_READINESS:-none}" = "state" ] && [ "$acked_prepare" -eq 0 ] &&
    [ "$(read_ready)" = "${LODE_INSTANCE:-}-1" ]; then
    log "prepare: lode prompted a staged update — preparing for cut-over"
    if [ "$BUILD_PREPARE_GATE" = "1" ]; then
      while [ "$running" -eq 1 ] && [ ! -f "${LODE_DATA_DIR:-}/prepare_ok" ]; do
        sleep 0.2 &
        wait "$!" 2>/dev/null || true
      done
    fi
    write_ready 2
    acked_prepare=1
  fi
  sleep 0.5 &
  wait "$!" 2>/dev/null || true
done
