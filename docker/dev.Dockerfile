# Build/run image for the whole project (INFO.md section 5a).
#
# Everything that compiles or runs dependency code happens in a container from this image,
# never on the host. The compose file (../compose.yaml) decides what each container can
# see: source read-only, .git hidden, and no network for anything that builds or tests.
#
# Base: the official Rust image, pinned by version tag AND digest so a re-tag can't swap
# the toolchain under us. 1.98.1 was released 2026-09-01 (older than the two-week rule).
FROM rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7

# Debian packages only (distro-signed): curl + jq for the REST history puller, gzip for
# recorder file rotation, ca-certificates for TLS, pkg-config/gcc already in the image.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl jq gzip git \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

# Supply-chain tools, built from source inside this image with their own lockfiles.
# Versions picked 2026-09-29: newest releases that are at least two weeks old.
RUN cargo install --locked cargo-audit --version 0.22.2 \
 && cargo install --locked cargo-deny --version 0.20.2 \
 && cargo install --locked cargo-vet --version 0.10.2 \
 && rm -rf /usr/local/cargo/registry /usr/local/cargo/git

# Containers run as the host user (so files they write belong to you, not root), so the
# writable locations must be world-writable. Named volumes mounted here inherit these
# permissions the first time they are created.
#   /cargo       CARGO_HOME: registry cache (only cargo itself writes it; compose.yaml)
#   /target      CARGO_TARGET_DIR: build output, kept out of the source tree
#   /advisories  RustSec databases for cargo-audit and cargo-deny
# cargo-vet keeps its cache under $HOME, so it lasts only for one run.
RUN mkdir -p /cargo /target /advisories && chmod 1777 /cargo /target /advisories
# HOME is a throwaway directory, not the shared cache volume, so no tool leaves dotfiles
# (~/.curlrc, ~/.jq) there for another container to read. /cargo/bin goes last on PATH:
# cargo would otherwise prefer cargo-<subcommand> binaries found there over the tools
# installed above.
# PERPS_IN_CONTAINER lets the host guard (tools/host-guard/) allow builds here.
ENV CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/target \
    HOME=/tmp \
    PATH=/usr/local/cargo/bin:$PATH:/cargo/bin \
    PERPS_IN_CONTAINER=1
WORKDIR /work
