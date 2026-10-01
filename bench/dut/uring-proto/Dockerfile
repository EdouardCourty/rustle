# io_uring prototype (monoio) of myproxy's HTTP/1 keep-alive hot path —
# measurement only. Needs io_uring syscalls: run with SECCOMP=unconfined
# (Docker's default seccomp profile blocks io_uring_*).
FROM rust:1-slim AS build
WORKDIR /build
ENV RUSTFLAGS="-C target-cpu=native"
COPY bench/uring-proto ./
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /build/target/release/uring-proto /usr/local/bin/uring-proto
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["uring-proto"]
