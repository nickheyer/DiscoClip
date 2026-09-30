# syntax=docker/dockerfile:1

# Builds the server, with the web app embedded, and runs it as an unprivileged user with
# the tools a full deployment needs beside it: a browser for pages that only play through
# a media source extension, and fonts for subtitles burnt into pictures.

FROM rust:1-bookworm AS build
# cmake, clang and libclang build the TLS library the HTTP client links; Node builds the
# web app.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg cmake clang libclang-dev pkg-config perl \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/crates/app/ui/node_modules \
    cargo build --release --locked \
    && cp target/release/discoclip /discoclip

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl tzdata \
        chromium \
        fontconfig fonts-dejavu-core fonts-noto-core fonts-noto-cjk \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 1000 --user-group --home-dir /var/lib/discoclip \
        --shell /usr/sbin/nologin discoclip \
    && mkdir -p /var/lib/discoclip /var/cache/discoclip \
    && chown discoclip:discoclip /var/lib/discoclip /var/cache/discoclip
COPY --from=build /discoclip /usr/local/bin/discoclip

# Where the database, the key, published files and backups live, where the cache and
# the unpacked ffmpeg live, and how the server listens inside the container. Every value
# can be overridden the same way, or in a mounted config file, or in the web app.
ENV DISCOCLIP_DATA_DIR=/var/lib/discoclip \
    DISCOCLIP_ENGINE__CACHE_DIR=/var/cache/discoclip \
    DISCOCLIP_LOCAL__DIR=/var/lib/discoclip/local \
    DISCOCLIP_BACKUP__DIR=/var/lib/discoclip/backups \
    DISCOCLIP_WEB__BIND=0.0.0.0:8080 \
    DISCOCLIP_ENGINE__BROWSER__EXECUTABLE=/usr/bin/chromium \
    DISCOCLIP_ENGINE__BROWSER__ARGS=--no-sandbox

USER discoclip
WORKDIR /var/lib/discoclip
VOLUME ["/var/lib/discoclip", "/var/cache/discoclip"]
EXPOSE 8080
# SIGTERM starts the graceful shutdown: running jobs get their grace period, the web
# listener drains, the bots close their gateway connections.
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=10s --start-period=90s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/discoclip"]
