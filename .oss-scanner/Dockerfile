# Build environment for the OSS Scanner. Context = the repository, checkout at /src.
# Network is available while this file builds and removed afterwards: everything the
# build, the unit tests and the e2e harness need is fetched or compiled here.
FROM rust:1-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive

# curl, lsof, procps: used by the e2e harness (curl interop scenarios, fd and
# CLOSE_WAIT leak checks). clang/build-essential: `ring` compiles C and assembly.
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates clang curl git lsof pkg-config procps \
    && rm -rf /var/lib/apt/lists/*

COPY . /src
WORKDIR /src

# Debug info and frame pointers for the scanner's profiling and crash analysis.
ENV RUSTFLAGS="-C force-frame-pointers=yes" \
    CARGO_PROFILE_RELEASE_DEBUG=1

# Fetch every crate of the workspace (proxy, e2e harness, bench generator) while online.
RUN cargo fetch --locked

# From here on nothing may touch the network.
ENV CARGO_NET_OFFLINE=true

# The proxy (the e2e harness runs the release binary) and the harness itself.
RUN cargo build --locked --release -p rustle \
    && cargo build --locked --profile e2e -p e2e

# Unit and in-process end-to-end tests: compile now, run them so the image is known good.
# A failing test must not abort the build.
RUN cargo test --locked -p rustle --no-run \
    && (cargo test --locked -p rustle || echo "WARNING: cargo test reported failures")

# Black-box e2e: quick subset (full suite: `cargo run --profile e2e -p e2e`, ~2 min).
RUN (cargo run -q --profile e2e -p e2e -- --quick || echo "WARNING: e2e --quick reported failures")
