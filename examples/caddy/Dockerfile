# syntax=docker/dockerfile:1
# Baseline for bench.sh: the upstream image at the version nixpkgs ships for
# `package = "caddy"`, configured like Russelfile.toml. It is a prebuilt image,
# so the baseline "build" is a pull (cold) or a cache hit (warm), not a compile.
FROM docker.io/library/caddy:2.11.4-alpine
EXPOSE 8080
CMD ["caddy", "file-server", "--listen", ":8080"]
