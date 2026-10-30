# Bench TLS certificate: same type for every proxy (ECDSA P-256,
# self-signed) so handshake costs compare. The generator does not verify it.
FROM alpine:3.20 AS cert
RUN apk add --no-cache openssl && openssl req -x509 -newkey ec \
    -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 -subj /CN=proxy \
    -keyout /key.pem -out /cert.pem && chmod 644 /key.pem

# Squid (C++, event loop per process), latest stable built from source with
# upstream's default optimization flags (the Debian package lags: 5.7 vs 7.x).
# Multi-core = SMP workers: WORKERS is written into the config. Caching
# disabled: pure forward proxy.
FROM debian:bookworm-slim AS build
ARG SQUID_VERSION=7.7
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential libssl-dev libltdl-dev libcap-dev pkg-config perl curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://github.com/squid-cache/squid/releases/download/SQUID_$(echo "${SQUID_VERSION}" | tr . _)/squid-${SQUID_VERSION}.tar.gz" | tar xz \
 && cd "squid-${SQUID_VERSION}" \
 && ./configure --prefix=/usr --sysconfdir=/etc/squid --localstatedir=/var --libexecdir=/usr/lib/squid \
        --with-openssl --with-default-user=proxy --enable-auth-basic=NCSA \
        --disable-snmp --disable-htcp --disable-wccp --disable-wccpv2 --disable-eui --disable-ident-lookups \
 && make -j"$(nproc)" && make install DESTDIR=/out

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends libssl3 libltdl7 libcap2 openssl \
 && rm -rf /var/lib/apt/lists/* \
 && install -d -o proxy -g proxy /var/run/squid /var/log/squid /var/cache/squid
COPY --from=build /out/usr /usr
COPY --from=build /out/etc /etc
RUN mkdir -p /etc/squid && printf 'bench:%s\n' "$(openssl passwd -1 secret1)" > /etc/squid/htpasswd
# /var/run/squid: SMP workers receive the listening socket from the
# coordinator over Unix sockets there — without it nobody listens.
COPY --from=cert /cert.pem /key.pem /etc/squid/
COPY benchmarks/proxies/squid/squid-on.conf /etc/squid/on.conf
COPY benchmarks/proxies/squid/squid-off.conf /etc/squid/off.conf
ENV AUTH=on WORKERS=1
# bench-ports: tls=8443 (no HTTP/2 towards clients)
EXPOSE 8080 8443
ENTRYPOINT ["sh", "-c", "sed \"s/^workers .*/workers ${WORKERS:-1}/\" /etc/squid/${AUTH:-on}.conf > /tmp/squid.conf && exec squid --foreground -f /tmp/squid.conf"]
