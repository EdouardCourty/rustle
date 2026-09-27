FROM alpine:3.20
RUN apk add --no-cache socat
EXPOSE 9000
CMD ["socat", "-u", "TCP-LISTEN:9000,reuseaddr,fork", "OPEN:/dev/null"]
