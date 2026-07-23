# syntax=docker/dockerfile:1
# Matches go.mod (1.22). In-memory URL shortener.
FROM docker.io/library/golang:1.22-alpine AS build
WORKDIR /app
COPY go.mod ./
RUN go mod download
COPY main.go ./
RUN CGO_ENABLED=0 go build -o shortlink .

FROM docker.io/library/alpine:3.21
COPY --from=build /app/shortlink /shortlink
RUN adduser -D -u 10001 app
USER app
EXPOSE 3000
ENV PORT=3000
CMD ["/shortlink"]
