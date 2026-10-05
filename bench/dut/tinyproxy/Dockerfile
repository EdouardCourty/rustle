# Tinyproxy (C, one thread per client), latest stable built from source (the
# Debian package lags). No upstream keep-alive: a documented limitation.
FROM debian:bookworm-slim AS build
ARG TINYPROXY_VERSION=1.11.3
RUN apt-get update && apt-get install -y --no-install-recommends build-essential curl ca-certificates xz-utils \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://github.com/tinyproxy/tinyproxy/releases/download/${TINYPROXY_VERSION}/tinyproxy-${TINYPROXY_VERSION}.tar.xz" | tar xJ \
 && cd "tinyproxy-${TINYPROXY_VERSION}" \
 && ./configure --prefix=/usr --sysconfdir=/etc/tinyproxy --disable-filter --disable-reverse --disable-transparent --disable-upstream \
 && make -j"$(nproc)" && make install DESTDIR=/out

FROM debian:bookworm-slim
COPY --from=build /out/usr /usr
COPY bench/dut/tinyproxy/tinyproxy.conf /etc/tinyproxy/base.conf
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "cp /etc/tinyproxy/base.conf /tmp/t.conf && if [ \"${AUTH:-on}\" = on ]; then echo 'BasicAuth bench secret1' >> /tmp/t.conf; fi && exec tinyproxy -d -c /tmp/t.conf"]
