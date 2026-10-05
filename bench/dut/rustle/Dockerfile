# Bench TLS certificate: same type for every DUT (ECDSA P-256,
# self-signed) so handshake costs compare. The generator does not verify it.
FROM alpine:3.20 AS cert
RUN apk add --no-cache openssl && openssl req -x509 -newkey ec \
    -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 -subj /CN=proxy \
    -keyout /key.pem -out /cert.pem && chmod 644 /key.pem

FROM rust:1-slim AS build
WORKDIR /build
# ALLOCATOR build arg: "" (glibc default) | --features jemalloc | --features mimalloc
ARG ALLOCATOR
# M10.3: native codegen — the image is built on the deployment box (bench
# rule: build where you run). Enables the ISA extensions of that CPU
# (AVX2/BMI on this class); rebuild on older hardware.
ENV RUSTFLAGS="-C target-cpu=native"
COPY Cargo.toml ./
COPY rustle ./rustle
COPY bench/generator ./bench/generator
# Workspace member, not built here: its manifest and a stub main suffice.
COPY e2e/Cargo.toml ./e2e/Cargo.toml
# jemalloc-sys compiles C from source: needs autoconf/make.
RUN if echo "$ALLOCATOR" | grep -q jemalloc; then \
        apt-get update && apt-get install -y --no-install-recommends autoconf make && \
        rm -rf /var/lib/apt/lists/*; fi
RUN mkdir -p rustle/src bench/generator/src e2e/src && \
    printf 'fn main() {}\n' > rustle/src/main.rs && \
    printf 'fn main() {}\n' > bench/generator/src/main.rs && \
    printf 'fn main() {}\n' > e2e/src/main.rs && \
    cargo build --release -p rustle
COPY rustle/src ./rustle/src
COPY bench/generator/src ./bench/generator/src
# Touch: source mtimes from the context can be older than the dummy-built
# artifacts, which would make cargo keep the dummy binaries (mtime trap).
RUN find rustle/src bench/generator/src -type f -exec touch {} + && \
    cargo build --release -p rustle $ALLOCATOR

FROM debian:bookworm-slim
COPY --from=build /build/target/release/rustle /usr/local/bin/rustle
COPY --from=cert /cert.pem /key.pem /etc/rustle/
COPY bench/dut/rustle/rustle-on.toml /etc/rustle/on.toml
COPY bench/dut/rustle/rustle-off.toml /etc/rustle/off.toml
ENV AUTH=on WORKERS=1 ACCESS_LOG=off KTLS=false
# bench-ports: tls=8443 h2=8443 (HTTP/2 via ALPN on the HTTPS listener)
EXPOSE 8080 8443
# WORKERS (multi-core runs), ACCESS_LOG (access log cost) and KTLS
# (kernel TLS A/B) are patched into the config by the harness.
ENTRYPOINT ["sh", "-c", "sed -e \"s/^workers = .*/workers = ${WORKERS:-1}/\" -e \"s/^access = .*/access = \\\"${ACCESS_LOG:-off}\\\"/\" -e \"s/^ktls = .*/ktls = ${KTLS:-false}/\" /etc/rustle/${AUTH:-on}.toml > /tmp/rustle.toml && exec rustle /tmp/rustle.toml"]
