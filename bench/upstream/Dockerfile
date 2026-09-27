FROM nginx:1.27-alpine
COPY nginx.conf /etc/nginx/nginx.conf
RUN head -c 1024 /dev/zero > /usr/share/nginx/html/1kb && \
    head -c 10240 /dev/zero > /usr/share/nginx/html/10kb && \
    head -c 1048576 /dev/zero > /usr/share/nginx/html/1mb
