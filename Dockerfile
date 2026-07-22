# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p plurxd && cp target/release/plurxd /plurxd

FROM debian:bookworm-slim
# plurxd shells out to ffmpeg/ffprobe for scanning, remux, and transcode; TLS
# roots are for TMDB/AniList. VA-API needs an actual driver in the image
# (--no-install-recommends skips them): iHD (Gen8+) + i965 (older) for Intel,
# Mesa for AMD — the non-free component carries the full Intel encoder
# support. Startup validation test-encodes, so only working paths are used.
# For the fanciest pipelines, mount a jellyfin-ffmpeg build and set
# PLURX_FFMPEG/PLURX_FFPROBE (see deploy/README.md).
RUN sed -i 's/Components: main/Components: main non-free non-free-firmware/' \
        /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates mesa-va-drivers \
    && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
        apt-get install -y --no-install-recommends \
            intel-media-va-driver-non-free i965-va-driver; \
    fi \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r plurx \
    && useradd -r -g plurx -d /var/lib/plurx plurx \
    && mkdir -p /var/lib/plurx \
    && chown plurx:plurx /var/lib/plurx
COPY --from=build /plurxd /usr/local/bin/plurxd

ENV PLURX_BIND=0.0.0.0:32600 \
    PLURX_DATA_DIR=/var/lib/plurx

EXPOSE 32600
VOLUME ["/var/lib/plurx"]
USER plurx

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD ["plurxd", "healthcheck"]

ENTRYPOINT ["plurxd"]
CMD ["run"]
