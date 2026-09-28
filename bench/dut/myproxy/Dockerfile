FROM rust:1-slim AS build
WORKDIR /build
COPY Cargo.toml ./
COPY myproxy ./myproxy
COPY bench/generator ./bench/generator
RUN mkdir -p myproxy/src bench/generator/src && \
    printf 'fn main() {}\n' > myproxy/src/main.rs && \
    printf 'fn main() {}\n' > bench/generator/src/main.rs && \
    cargo build --release -p myproxy
COPY myproxy/src ./myproxy/src
COPY bench/generator/src ./bench/generator/src
# Touch: source mtimes from the context can be older than the dummy-built
# artifacts, which would make cargo keep the dummy binaries (mtime trap).
RUN find myproxy/src bench/generator/src -type f -exec touch {} + && \
    cargo build --release -p myproxy

FROM debian:bookworm-slim
COPY --from=build /build/target/release/myproxy /usr/local/bin/myproxy
COPY bench/dut/myproxy/myproxy-on.toml /etc/myproxy/on.toml
COPY bench/dut/myproxy/myproxy-off.toml /etc/myproxy/off.toml
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "exec myproxy /etc/myproxy/${AUTH:-on}.toml"]
