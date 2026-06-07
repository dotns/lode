# Local, prebuilt manifest/artifact server image for the docker-compose
# integration test (tests/src/integration/compose.test.ts). Built from the static,
# std-only `lodetest` binary (tests/compose/lodetest) staged into the build
# context — NOT pulled from a registry, so the test needs no real network.
#
# `lodetest serve /www 8080` is a read-fresh HTTP file server over a writable
# volume mounted at /www: the test publishes a new lode/v1 manifest or signed
# artifact simply by writing the file, and the next fetch serves it.
#
# distroless/static (no libc/shell), so `lodetest` MUST be a static binary; the
# test builds it with `+crt-static` (or musl). The lode service containers reach
# this server over a fixed compose IP (no DNS), so the static binary needs no NSS.
FROM gcr.io/distroless/static:latest
COPY lodetest /lodetest
EXPOSE 8080
ENTRYPOINT ["/lodetest", "serve", "/www", "8080"]
