# syntax=docker/dockerfile:1
# Pure-std Rust HTTP (no crates.io). Build with Alpine rustc (same idea as flake).
# Runtime needs libgcc for rustc's unwind symbols (exit 127 without it).
FROM docker.io/library/alpine:3.21 AS build
RUN apk add --no-cache rust
WORKDIR /app
COPY src/main.rs ./src/main.rs
RUN rustc -C opt-level=2 -o hello-rust src/main.rs

FROM docker.io/library/alpine:3.21
RUN apk add --no-cache libgcc
COPY --from=build /app/hello-rust /hello-rust
RUN adduser -D -u 10001 app
USER app
EXPOSE 3000
ENV PORT=3000
CMD ["/hello-rust"]
