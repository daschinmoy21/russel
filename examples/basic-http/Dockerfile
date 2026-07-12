# syntax=docker/dockerfile:1
# ponytail: matches go.mod (1.22) so the binary is the same toolchain as Nix build.
FROM docker.io/library/golang:1.22-alpine AS build
WORKDIR /app
COPY go.mod main.go ./
COPY static ./static
RUN CGO_ENABLED=0 go build -o basic-http .

FROM docker.io/library/alpine:3.21
# ponytail: no ca-certificates — this server serves localhost only, no TLS calls.
COPY --from=build /app/basic-http /basic-http
RUN adduser -D -u 10001 app
USER app
EXPOSE 3000
ENV PORT=3000
CMD ["/basic-http"]
