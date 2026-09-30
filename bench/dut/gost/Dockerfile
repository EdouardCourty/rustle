# gost v3 (Go): HTTP proxy with Basic auth. The Go runtime sizes
# GOMAXPROCS from the CPU affinity (Docker cpuset): WORKERS is not needed.
FROM gogost/gost:3.2.6
ENV AUTH=on
EXPOSE 8080
ENTRYPOINT ["sh", "-c", "if [ \"${AUTH:-on}\" = on ]; then exec gost -L http://bench:secret1@:8080; else exec gost -L http://:8080; fi"]
