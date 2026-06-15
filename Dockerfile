# Built by .github/workflows/docker.yml from prebuilt release binaries.
# The build context holds linux-amd64/lode and linux-arm64/lode (extracted from
# the GitHub release tarballs); buildx sets TARGETARCH per platform.
#
# Prep stage: stage the binary once and create the `lode-cli` symlink here, so
# the final image carries ONE copy of the binary instead of two. ubase is kept
# minimal (no guaranteed shell/ln at build time), so the symlink is made in a
# digest-pinned alpine and carried over — symlinks survive COPY --from.
FROM alpine@sha256:a2d49ea686c2adfe3c992e47dc3b5e7fa6e6b5055609400dc2acaeb241c829f4 AS prep
ARG TARGETARCH
COPY linux-${TARGETARCH}/lode /staging/lode
RUN ln -s lode /staging/lode-cli

# Base: zzci/ubase — a general-purpose image (glibc, a shell, common tools), not a
# minimal/static one, so this same image can also host script apps whose runtime
# (bun/node/deno) lode downloads at boot into its runtime cache. lode itself is a
# musl-static binary that runs on any base, and its TLS roots are bundled
# (webpki-roots), so no system ca-certificates are required. Pinned by digest for
# reproducible image builds; the digest is the multi-arch index
# (linux/amd64 + linux/arm64) matching the platforms buildx targets below —
# re-resolve it when bumping the base.
FROM zzci/ubase@sha256:6eb4065a1481c976afc8023bb97f0386ab4b1667664d35dfbcc645feb4e0340d
# lode is a multi-call binary: as `lode` it is the loader (the entrypoint); as
# `lode-cli` (same binary, different name) it is the operator/publisher toolkit,
# reachable via `docker exec <container> lode-cli status`. Both go on PATH at
# /usr/bin so downstream images and `docker exec` can call them by bare name.
COPY --from=prep /staging/ /usr/bin/
ENTRYPOINT ["/usr/bin/lode"]
