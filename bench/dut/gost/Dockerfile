# gost v3 (Go): HTTP proxy with Basic auth. The Go runtime sizes
# GOMAXPROCS from the CPU affinity (Docker cpuset): WORKERS is not needed.
# Bench TLS certificate: same type for every DUT (ECDSA P-256,
# self-signed) so handshake costs compare. The generator does not verify it.
FROM alpine:3.20 AS cert
RUN apk add --no-cache openssl && openssl req -x509 -newkey ec \
    -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 -subj /CN=proxy \
    -keyout /key.pem -out /cert.pem && chmod 644 /key.pem

FROM gogost/gost:3.2.6
COPY --from=cert /cert.pem /key.pem /etc/gost/
# Access logs off, like the other DUTs (gost logs every connection at
# info: per-connection I/O the others do not pay).
ENV AUTH=on GOST_LOGGER_LEVEL=error
# bench-ports: tls=8443 h2=8444 (dedicated http2 listener)
EXPOSE 8080 8443 8444
ENTRYPOINT ["sh", "-c", "if [ \"${AUTH:-on}\" = on ]; then exec gost -L http://bench:secret1@:8080 -L \"https://bench:secret1@:8443?certFile=/etc/gost/cert.pem&keyFile=/etc/gost/key.pem\" -L \"http2://bench:secret1@:8444?certFile=/etc/gost/cert.pem&keyFile=/etc/gost/key.pem\"; else exec gost -L http://:8080 -L \"https://:8443?certFile=/etc/gost/cert.pem&keyFile=/etc/gost/key.pem\" -L \"http2://:8444?certFile=/etc/gost/cert.pem&keyFile=/etc/gost/key.pem\"; fi"]
