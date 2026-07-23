# syntax=docker/dockerfile:1
# Matches go.mod (1.22). Env/secrets demo — secret values come from the host at
# Russel deploy time; plain Docker/Podman runs without DEMO_SECRET set.
FROM docker.io/library/golang:1.22-alpine AS build
WORKDIR /app
COPY go.mod ./
RUN go mod download
COPY main.go ./
RUN CGO_ENABLED=0 go build -o env-config .

FROM docker.io/library/alpine:3.21
COPY --from=build /app/env-config /env-config
RUN adduser -D -u 10001 app
USER app
EXPOSE 3000
ENV PORT=3000
# Optional demo defaults (Russel injects [service.env] + secrets instead):
ENV GREETING="hello from Dockerfile" LOG_LEVEL=info
CMD ["/env-config"]
