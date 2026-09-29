# syntax=docker/dockerfile:1
# Baseline for bench.sh: the upstream image at the version nixpkgs ships for
# `package = "redis"`, configured like Russelfile.toml. It is a prebuilt image,
# so the baseline "build" is a pull (cold) or a cache hit (warm), not a compile.
FROM docker.io/library/redis:8.10.1-alpine
EXPOSE 6379
CMD ["redis-server", "--dir", "/data", "--bind", "0.0.0.0", "--requirepass", "demo-only-not-for-production"]
