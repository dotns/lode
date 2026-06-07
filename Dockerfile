# Built by .github/workflows/docker.yml from prebuilt release binaries.
# The build context holds linux-amd64/lode and linux-arm64/lode (extracted from
# the GitHub release tarballs); buildx sets TARGETARCH per platform.
# Base pinned by digest for reproducible builds. This is the multi-arch index
# (manifest list) digest of gcr.io/distroless/static:latest; buildx selects the
# matching per-platform manifest via TARGETARCH at build time.
# Resolved 2026-06-06 with:
#   docker buildx imagetools inspect gcr.io/distroless/static:latest
# To refresh: re-run that command and replace the digest below with its
# top-level "Digest:" value.
FROM gcr.io/distroless/static:latest@sha256:3592aa8171c77482f62bbc4164e6a2d141c6122554ace66e5cc910cadb961ff0
ARG TARGETARCH
# lode is a multi-call binary: as `lode` it is the loader (the entrypoint); as
# `lode-cli` (same binary, different name) it is the operator/publisher toolkit,
# reachable via `docker exec <container> /lode-cli status`.
COPY linux-${TARGETARCH}/lode /lode
COPY linux-${TARGETARCH}/lode /lode-cli
ENTRYPOINT ["/lode"]
