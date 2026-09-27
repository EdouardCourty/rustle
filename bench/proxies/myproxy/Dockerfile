FROM rust:1-slim AS build
WORKDIR /build
COPY Cargo.toml ./
COPY myproxy ./myproxy
COPY bench/client ./bench/client
RUN mkdir -p myproxy/src bench/client/src && \
    printf 'fn main() {}\n' > myproxy/src/main.rs && \
    printf 'fn main() {}\n' > bench/client/src/main.rs && \
    cargo build --release -p myproxy
COPY myproxy/src ./myproxy/src
COPY bench/client/src ./bench/client/src
# Touch: source mtimes from the context can be older than the dummy-built
# artifacts, which would make cargo keep the dummy binaries (mtime trap).
RUN find myproxy/src bench/client/src -type f -exec touch {} + && \
    cargo build --release -p myproxy

FROM debian:bookworm-slim
COPY --from=build /build/target/release/myproxy /usr/local/bin/myproxy
COPY bench/proxies/myproxy/myproxy-on.toml /etc/myproxy/on.toml
COPY bench/proxies/myproxy/myproxy-off.toml /etc/myproxy/off.toml
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "exec myproxy /etc/myproxy/${AUTH:-on}.toml"]
