# Bench TLS certificate: same type for every DUT (ECDSA P-256,
# self-signed) so handshake costs compare. The generator does not verify it.
FROM alpine:3.20 AS cert
RUN apk add --no-cache openssl && openssl req -x509 -newkey ec \
    -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 -subj /CN=proxy \
    -keyout /key.pem -out /cert.pem && chmod 644 /key.pem

# Squid (C++, event loop per process). Multi-core = SMP workers: WORKERS
# is written into the config. Caching disabled: pure forward proxy.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends squid-openssl openssl \
 && rm -rf /var/lib/apt/lists/* \
 && printf 'bench:%s\n' "$(openssl passwd -1 secret1)" > /etc/squid/htpasswd \
 && install -d -o proxy -g proxy /var/run/squid
# /var/run/squid: SMP workers receive the listening socket from the
# coordinator over Unix sockets there — without it nobody listens.
COPY --from=cert /cert.pem /key.pem /etc/squid/
COPY bench/dut/squid/squid-on.conf /etc/squid/on.conf
COPY bench/dut/squid/squid-off.conf /etc/squid/off.conf
ENV AUTH=on WORKERS=1
# bench-ports: tls=8443 (no HTTP/2 towards clients)
EXPOSE 8080 8443
ENTRYPOINT ["sh", "-c", "sed \"s/^workers .*/workers ${WORKERS:-1}/\" /etc/squid/${AUTH:-on}.conf > /tmp/squid.conf && exec squid --foreground -f /tmp/squid.conf"]
