# syntax=docker/dockerfile:1
# Baseline for bench.sh: the upstream image at the version nixpkgs ships for
# `package = "postgresql"`, configured like Russelfile.toml. It is a prebuilt image,
# so the baseline "build" is a pull (cold) or a cache hit (warm), not a compile.
FROM docker.io/library/postgres:18.6-alpine
# The image runs initdb on first start (the Russel race uses a PGDATA the
# bench prepared beforehand), so this baseline includes initdb time.
# Trust auth is for the local bench only.
ENV POSTGRES_HOST_AUTH_METHOD=trust
EXPOSE 5432
