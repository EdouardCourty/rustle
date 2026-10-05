FROM rust:1-slim AS build
WORKDIR /build
COPY . /build
# Standalone build (this directory is its own cargo root): the profile is
# set through the environment, matching the workspace's release profile.
# NATIVE=1 adds -C target-cpu=native (build on the machine that runs it).
ARG NATIVE=0
ENV CARGO_PROFILE_RELEASE_LTO=fat \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    CARGO_PROFILE_RELEASE_PANIC=abort
RUN if [ "$NATIVE" = 1 ]; then export RUSTFLAGS="-C target-cpu=native"; fi; \
    cargo build --release

FROM debian:bookworm-slim
COPY --from=build /build/target/release/bench-client /usr/local/bin/bench-client
ENTRYPOINT ["bench-client"]
