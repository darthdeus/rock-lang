# Clean-room headless test environment for rock.nvim.
#
# Pinned to nvim 0.12.2 — matches the version we develop against.
# Image carries only nvim + the bare runtime; the plugin dir is mounted
# at /plugin and tests/run.sh is the entrypoint.
#
# TARGETARCH is supplied by buildkit; we map it to the matching
# upstream tarball so the image runs natively on both linux/amd64 and
# linux/arm64 (Apple Silicon hosts).
FROM --platform=$BUILDPLATFORM debian:bookworm-slim

ARG TARGETARCH
ARG NVIM_VERSION=v0.12.2

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates curl xz-utils \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
        amd64) tarball=nvim-linux-x86_64.tar.gz; dir=nvim-linux-x86_64 ;; \
        arm64) tarball=nvim-linux-arm64.tar.gz; dir=nvim-linux-arm64 ;; \
        *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/nvim.tar.gz \
        "https://github.com/neovim/neovim/releases/download/${NVIM_VERSION}/${tarball}"; \
    tar -C /opt -xzf /tmp/nvim.tar.gz; \
    ln -s "/opt/${dir}/bin/nvim" /usr/local/bin/nvim; \
    rm /tmp/nvim.tar.gz

WORKDIR /plugin
ENTRYPOINT ["tests/run.sh"]
