# syntax=docker/dockerfile:1
# Baseline for bench.sh: the upstream image at the version nixpkgs ships for
# `package = "meilisearch"`, configured like Russelfile.toml. It is a prebuilt image,
# so the baseline "build" is a pull (cold) or a cache hit (warm), not a compile.
FROM docker.io/getmeili/meilisearch:v1.53.2
ENV MEILI_MASTER_KEY=demo-only-not-for-production \
    MEILI_DB_PATH=/data \
    MEILI_DUMP_DIR=/data/dumps \
    MEILI_SNAPSHOT_DIR=/data/snapshots \
    MEILI_HTTP_ADDR=0.0.0.0:7700
EXPOSE 7700
