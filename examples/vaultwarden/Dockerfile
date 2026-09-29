# syntax=docker/dockerfile:1
# Baseline for bench.sh: the upstream image at the version nixpkgs ships for
# `package = "vaultwarden"`, configured like Russelfile.toml. It is a prebuilt image,
# so the baseline "build" is a pull (cold) or a cache hit (warm), not a compile.
FROM docker.io/vaultwarden/server:1.37.3-alpine
# The image listens on :80; the Russelfile uses Rocket's default 8000 and
# serves the API only. The image refuses to start unless /data is a mounted
# volume; the bench baseline runs without volumes, so opt out.
ENV DATA_FOLDER=/data \
    WEB_VAULT_ENABLED=false \
    ROCKET_ADDRESS=0.0.0.0 \
    ROCKET_PORT=8000 \
    I_REALLY_WANT_VOLATILE_STORAGE=true
EXPOSE 8000
