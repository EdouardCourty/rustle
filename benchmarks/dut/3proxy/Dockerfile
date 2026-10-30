FROM alpine:3.20 AS build
RUN apk add --no-cache build-base curl
ARG VERSION=1.0.0
RUN curl -fsSL -o 3proxy.tar.gz "https://github.com/3proxy/3proxy/archive/refs/tags/${VERSION}.tar.gz" \
 && tar xzf 3proxy.tar.gz \
 && cd "3proxy-${VERSION}" \
 && make -f Makefile.Linux -j"$(nproc)"

FROM alpine:3.20
RUN apk add --no-cache libgcc
ARG VERSION=1.0.0
COPY --from=build /3proxy-${VERSION}/bin/3proxy /usr/local/bin/3proxy
COPY benchmarks/dut/3proxy/3proxy-on.cfg /etc/3proxy/on.cfg
COPY benchmarks/dut/3proxy/3proxy-off.cfg /etc/3proxy/off.cfg
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "exec 3proxy /etc/3proxy/${AUTH:-on}.cfg"]
