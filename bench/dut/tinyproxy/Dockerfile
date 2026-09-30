# Tinyproxy (C, one thread per client). No upstream keep-alive.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends tinyproxy \
 && rm -rf /var/lib/apt/lists/*
COPY bench/dut/tinyproxy/tinyproxy.conf /etc/tinyproxy/base.conf
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "cp /etc/tinyproxy/base.conf /tmp/t.conf && if [ \"${AUTH:-on}\" = on ]; then echo 'BasicAuth bench secret1' >> /tmp/t.conf; fi && exec tinyproxy -d -c /tmp/t.conf"]
