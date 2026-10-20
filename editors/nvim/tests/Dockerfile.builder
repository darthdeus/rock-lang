# Builder image for rock-lsp. Used by tests/build-rock-lsp.sh.
#
# Stays separate from the test runtime image so the test container can
# remain lean (~70 MB) while this one carries the full rust toolchain
# (~1.5 GB). Cargo target dir is mounted from the host cache so
# incremental builds are fast across runs.
FROM rust:1-bookworm

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
