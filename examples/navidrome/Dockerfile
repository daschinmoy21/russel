# syntax=docker/dockerfile:1
# Baseline for bench.sh: the upstream image at the version nixpkgs ships for
# `package = "navidrome"`, configured like Russelfile.toml. It is a prebuilt image,
# so the baseline "build" is a pull (cold) or a cache hit (warm), not a compile.
FROM docker.io/deluan/navidrome:0.64.1
ENV ND_DATAFOLDER=/data \
    ND_MUSICFOLDER=/music
EXPOSE 4533
