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
COPY myproxy ./myproxy
COPY bench/generator ./bench/generator
# jemalloc-sys compiles C from source: needs autoconf/make.
RUN if echo "$ALLOCATOR" | grep -q jemalloc; then \
        apt-get update && apt-get install -y --no-install-recommends autoconf make && \
        rm -rf /var/lib/apt/lists/*; fi
RUN mkdir -p myproxy/src bench/generator/src && \
    printf 'fn main() {}\n' > myproxy/src/main.rs && \
    printf 'fn main() {}\n' > bench/generator/src/main.rs && \
    cargo build --release -p myproxy
COPY myproxy/src ./myproxy/src
COPY bench/generator/src ./bench/generator/src
# Touch: source mtimes from the context can be older than the dummy-built
# artifacts, which would make cargo keep the dummy binaries (mtime trap).
RUN find myproxy/src bench/generator/src -type f -exec touch {} + && \
    cargo build --release -p myproxy $ALLOCATOR

FROM debian:bookworm-slim
COPY --from=build /build/target/release/myproxy /usr/local/bin/myproxy
COPY --from=cert /cert.pem /key.pem /etc/myproxy/
COPY bench/dut/myproxy/myproxy-on.toml /etc/myproxy/on.toml
COPY bench/dut/myproxy/myproxy-off.toml /etc/myproxy/off.toml
ENV AUTH=on WORKERS=1 ACCESS_LOG=off
# bench-ports: tls=8443 h2=8443 (HTTP/2 via ALPN on the HTTPS listener)
EXPOSE 8080 8443
# WORKERS (multi-core runs) and ACCESS_LOG (access log cost) are patched
# into the config by the harness.
ENTRYPOINT ["sh", "-c", "sed -e \"s/^workers = .*/workers = ${WORKERS:-1}/\" -e \"s/^access = .*/access = \\\"${ACCESS_LOG:-off}\\\"/\" /etc/myproxy/${AUTH:-on}.toml > /tmp/myproxy.toml && exec myproxy /tmp/myproxy.toml"]
