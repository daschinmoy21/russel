# syntax=docker/dockerfile:1
FROM docker.io/library/python:3.13-alpine
WORKDIR /app
COPY index.html ./
RUN adduser -D -u 10001 app && chown app /app/index.html
USER app
EXPOSE 8000
ENV PORT=8000
CMD python3 -m http.server "$PORT"
