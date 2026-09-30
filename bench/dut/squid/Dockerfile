# Squid (C++, event loop per process). Multi-core = SMP workers: WORKERS
# is written into the config. Caching disabled: pure forward proxy.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends squid openssl \
 && rm -rf /var/lib/apt/lists/* \
 && printf 'bench:%s\n' "$(openssl passwd -1 secret1)" > /etc/squid/htpasswd \
 && install -d -o proxy -g proxy /var/run/squid
# /var/run/squid: SMP workers receive the listening socket from the
# coordinator over Unix sockets there — without it nobody listens.
COPY bench/dut/squid/squid-on.conf /etc/squid/on.conf
COPY bench/dut/squid/squid-off.conf /etc/squid/off.conf
ENV AUTH=on WORKERS=1
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "sed \"s/^workers .*/workers ${WORKERS:-1}/\" /etc/squid/${AUTH:-on}.conf > /tmp/squid.conf && exec squid --foreground -f /tmp/squid.conf"]
