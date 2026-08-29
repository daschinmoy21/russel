# syntax=docker/dockerfile:1
# Optional / bench.sh image. Russel deploy uses nixpkgs filebrowser via flake.nix.
# Binary from GitHub releases (not in Alpine repos). ADD so the layer is cacheable.
FROM docker.io/library/alpine:3.21 AS build
RUN apk add --no-cache ca-certificates
ARG FILEBROWSER_VERSION=v2.31.2
# sha256 of linux-amd64-filebrowser.tar.gz from the v2.31.2 GitHub release.
ARG FILEBROWSER_SHA256=a4eb80ffa5845597dc50a53f40ca230636a7c957ba97390c659bced3bab1c8f1
ADD https://github.com/filebrowser/filebrowser/releases/download/${FILEBROWSER_VERSION}/linux-amd64-filebrowser.tar.gz /tmp/filebrowser.tar.gz
RUN echo "${FILEBROWSER_SHA256}  /tmp/filebrowser.tar.gz" | sha256sum -c \
    && tar xzf /tmp/filebrowser.tar.gz -C /usr/local/bin filebrowser \
    && rm /tmp/filebrowser.tar.gz

FROM docker.io/library/alpine:3.21
RUN apk add --no-cache ca-certificates
COPY --from=build /usr/local/bin/filebrowser /usr/local/bin/filebrowser
COPY entrypoint.sh /usr/local/bin/filebrowser-entry
RUN chmod 0755 /usr/local/bin/filebrowser-entry \
    && adduser -D -u 10001 app
USER app
EXPOSE 8080
ENV PORT=8080
# Auth on. Bind 0.0.0.0 in the guest so -p works. Demo password is public;
# override FILEBROWSER_PASSWORD. Do not use --noauth. Do not publish the host
# port on 0.0.0.0.
CMD ["/usr/local/bin/filebrowser-entry"]
