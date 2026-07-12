# syntax=docker/dockerfile:1
# ponytail: filebrowser binary downloaded from GitHub releases (not in Alpine repos).
FROM docker.io/library/alpine:3.21 AS build
RUN apk add --no-cache curl ca-certificates
ARG FILEBROWSER_VERSION=v2.31.2
RUN curl -fsSL "https://github.com/filebrowser/filebrowser/releases/download/${FILEBROWSER_VERSION}/linux-amd64-filebrowser.tar.gz" \
  | tar xz -C /usr/local/bin filebrowser

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
