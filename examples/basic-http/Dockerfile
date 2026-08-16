# syntax=docker/dockerfile:1
FROM docker.io/library/golang:1.22-alpine AS build
WORKDIR /app
COPY go.mod ./
RUN go mod download
COPY main.go ./
COPY static ./static
RUN CGO_ENABLED=0 go build -o basic-http .

FROM docker.io/library/alpine:3.21
COPY --from=build /app/basic-http /basic-http
RUN adduser -D -u 10001 app
USER app
EXPOSE 3000
ENV PORT=3000
CMD ["/basic-http"]
