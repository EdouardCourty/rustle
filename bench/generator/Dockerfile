FROM rust:1-slim AS build
WORKDIR /build
COPY . /build
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /build/target/release/bench-client /usr/local/bin/bench-client
ENTRYPOINT ["bench-client"]
