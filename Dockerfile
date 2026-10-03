# syntax=docker/dockerfile:1

# DISCOCLIP CONTAINER IMAGE(S)
ARG RUST_IMAGE=rust:1-bookworm
ARG NODE_IMAGE=node:22-bookworm-slim
ARG RUNTIME_IMAGE=debian:trixie-slim

FROM ${NODE_IMAGE} AS node

FROM ${RUST_IMAGE} AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates cmake clang libclang-dev pkg-config perl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/crates/app/ui/node_modules \
    cargo build --release --locked \
    && cp target/release/discoclip /discoclip

FROM ${RUNTIME_IMAGE} AS ffmpeg-build
ARG FFMPEG_VERSION=7.1.5
ARG FFMPEG_SHA256=de668509caf9e35e3cd162473441fdb29538c6d96ed080292b3cf9e6fc5d558f
ARG NV_CODEC_HEADERS=n13.1.15.0
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential clang pkg-config nasm git ca-certificates curl xz-utils \
        libgnutls28-dev libxml2-dev \
        libass-dev libfreetype-dev libfontconfig-dev libfribidi-dev libharfbuzz-dev \
        libx264-dev libx265-dev libvpx-dev libsvtav1enc-dev libaom-dev libdav1d-dev \
        libopus-dev libvorbis-dev libmp3lame-dev \
        libzimg-dev libplacebo-dev libvulkan-dev libshaderc-dev \
        libva-dev libdrm-dev \
    && if [ "$(dpkg --print-architecture)" = amd64 ]; then \
        apt-get install -y --no-install-recommends libvpl-dev; \
    fi \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN git clone --depth 1 --branch "${NV_CODEC_HEADERS}" \
        https://github.com/FFmpeg/nv-codec-headers.git \
    && make -C nv-codec-headers install PREFIX=/usr/local \
    && rm -rf nv-codec-headers
RUN curl -fsSLo ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  ffmpeg.tar.xz" | sha256sum -c - \
    && tar -xJf ffmpeg.tar.xz --strip-components=1 \
    && rm ffmpeg.tar.xz
# Quick Sync goes through Intel's VPL, which Debian packages for amd64 only.
RUN set -eu; \
    vpl=""; \
    if [ "$(dpkg --print-architecture)" = amd64 ]; then vpl="--enable-libvpl"; fi; \
    PKG_CONFIG_PATH=/usr/local/lib/pkgconfig ./configure \
        --prefix=/usr/local \
        --extra-version=discoclip \
        --disable-debug --disable-doc --disable-ffplay \
        --enable-gpl \
        --enable-gnutls --enable-libxml2 \
        --enable-libass --enable-libfreetype --enable-libfontconfig --enable-libfribidi --enable-libharfbuzz \
        --enable-libx264 --enable-libx265 --enable-libvpx --enable-libsvtav1 --enable-libaom --enable-libdav1d \
        --enable-libopus --enable-libvorbis --enable-libmp3lame \
        --enable-libzimg --enable-libplacebo --enable-vulkan --enable-libshaderc \
        --enable-vaapi --enable-libdrm $vpl \
        --enable-ffnvcodec --enable-nvenc --enable-nvdec --enable-cuvid --enable-cuda-llvm \
    && make -j"$(nproc)" \
    && make install
# The Debian packages the two binaries link, for the gpu stage to install.
RUN ldd /usr/local/bin/ffmpeg /usr/local/bin/ffprobe \
    | awk '/=>/ { print $3 }' | xargs -r realpath | sort -u \
    | xargs -r dpkg -S | cut -d: -f1 | sort -u > /ffmpeg-runtime-deps.txt \
    && test -s /ffmpeg-runtime-deps.txt

FROM ${RUNTIME_IMAGE} AS runtime
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl tzdata tini \
        chromium \
        fontconfig fonts-dejavu-core fonts-noto-core fonts-noto-cjk \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 1000 --user-group --home-dir /var/lib/discoclip \
        --shell /usr/sbin/nologin discoclip \
    && mkdir -p /var/lib/discoclip /var/cache/discoclip \
    && chown discoclip:discoclip /var/lib/discoclip /var/cache/discoclip
COPY --from=build /discoclip /usr/local/bin/discoclip

ENV DISCOCLIP_DATA_DIR=/var/lib/discoclip \
    DISCOCLIP_ENGINE__CACHE_DIR=/var/cache/discoclip \
    DISCOCLIP_LOCAL__DIR=/var/lib/discoclip/local \
    DISCOCLIP_ENGINE__ARCHIVE__DIR=/var/lib/discoclip/archive \
    DISCOCLIP_BACKUP__DIR=/var/lib/discoclip/backups \
    DISCOCLIP_WEB__BIND=0.0.0.0:8080 \
    DISCOCLIP_ENGINE__BROWSER__EXECUTABLE=/usr/bin/chromium \
    DISCOCLIP_ENGINE__BROWSER__ARGS=--no-sandbox

LABEL org.opencontainers.image.title="DiscoClip" \
      org.opencontainers.image.description="Watches Discord channels for video links, then downloads, transcodes, posts, and archives each one" \
      org.opencontainers.image.source="https://github.com/nickheyer/DiscoClip" \
      org.opencontainers.image.licenses="MIT"

USER discoclip
WORKDIR /var/lib/discoclip
VOLUME ["/var/lib/discoclip", "/var/cache/discoclip"]
EXPOSE 8080
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=10s --start-period=90s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/discoclip"]

FROM runtime AS gpu
USER root
COPY --from=ffmpeg-build /ffmpeg-runtime-deps.txt /usr/local/share/discoclip/ffmpeg-runtime-deps.txt
RUN sed -i 's/^Components: main$/Components: main non-free non-free-firmware/' \
        /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        $(cat /usr/local/share/discoclip/ffmpeg-runtime-deps.txt) \
        libvulkan1 mesa-vulkan-drivers vulkan-tools \
        mesa-va-drivers vainfo \
    && if [ "$(dpkg --print-architecture)" = amd64 ]; then \
        apt-get install -y --no-install-recommends intel-media-va-driver-non-free libmfx-gen1.2; \
    fi \
    && rm -rf /var/lib/apt/lists/*
COPY --from=ffmpeg-build /usr/local/bin/ffmpeg /usr/local/bin/ffprobe /usr/local/bin/
ENV DISCOCLIP_ENGINE__FFMPEG=/usr/local/bin/ffmpeg \
    NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility,video,graphics
USER discoclip

FROM scratch AS binary
COPY --from=build /discoclip /discoclip

# DEFAULT TARGET
FROM runtime AS cpu
