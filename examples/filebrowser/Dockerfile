# syntax=docker/dockerfile:1
FROM docker.io/library/alpine:3.21 AS build
RUN apk add --no-cache ca-certificates
ARG FILEBROWSER_VERSION=v2.31.2
ADD https://github.com/filebrowser/filebrowser/releases/download/${FILEBROWSER_VERSION}/linux-amd64-filebrowser.tar.gz /tmp/filebrowser.tar.gz
RUN tar xzf /tmp/filebrowser.tar.gz -C /usr/local/bin filebrowser && rm /tmp/filebrowser.tar.gz

FROM docker.io/library/alpine:3.21
RUN apk add --no-cache ca-certificates
COPY --from=build /usr/local/bin/filebrowser /usr/local/bin/filebrowser
RUN adduser -D -u 10001 app
USER app
EXPOSE 8080
ENV PORT=8080
CMD mkdir -p /tmp/files && \
    echo "Welcome to Russel File Manager inside your Container!" > /tmp/files/README.txt && \
    echo "This container booted quickly using a lightweight Alpine image." >> /tmp/files/README.txt && \
    filebrowser --noauth --address 0.0.0.0 --port "$PORT" --database /tmp/filebrowser.db --root /tmp/files
