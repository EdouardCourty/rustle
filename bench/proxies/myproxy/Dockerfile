FROM rust:1-slim AS build
WORKDIR /build
COPY Cargo.toml ./
COPY myproxy ./myproxy
COPY bench/client ./bench/client
RUN mkdir -p myproxy/src bench/client/src && \
    printf 'fn main() {}\n' > myproxy/src/main.rs && \
    printf 'fn main() {}\n' > bench/client/src/main.rs && \
    cargo build --release -p myproxy && \
    rm myproxy/src/main.rs bench/client/src/main.rs
COPY myproxy/src ./myproxy/src
RUN cargo build --release -p myproxy

FROM debian:bookworm-slim
COPY --from=build /build/target/release/myproxy /usr/local/bin/myproxy
COPY bench/proxies/myproxy/myproxy-on.toml /etc/myproxy/on.toml
COPY bench/proxies/myproxy/myproxy-off.toml /etc/myproxy/off.toml
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "exec myproxy /etc/myproxy/${AUTH:-on}.toml"]
