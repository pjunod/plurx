# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p plurxd && cp target/release/plurxd /plurxd

FROM debian:bookworm-slim
# plurxd shells out to ffmpeg/ffprobe for scanning, remux, and transcode; TLS
# roots are for TMDB/AniList.
#
# Hardware transcode ships two ways in one image:
#   * jellyfin-ffmpeg (the DEFAULT engine, via PLURX_FFMPEG below) — bundles a
#     CURRENT Intel media driver + libva + oneVPL, so recent GPUs (Arc,
#     Meteor/Arrow Lake on the `xe` driver) that Debian's own driver is years
#     too old for can still do QSV/VAAPI. It's a full ffmpeg, so it also
#     handles scanning and software encode.
#   * the distro ffmpeg + Mesa/Intel VA drivers — the fallback if you override
#     PLURX_FFMPEG back to plain `ffmpeg` (older, widely-tested GPUs).
# Startup validation test-encodes each path, so only what actually works is used.
RUN sed -i 's/Components: main/Components: main non-free non-free-firmware/' \
        /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates mesa-va-drivers curl gnupg \
    && if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
        apt-get install -y --no-install-recommends \
            intel-media-va-driver-non-free i965-va-driver; \
    fi \
    && install -d /etc/apt/keyrings \
    && curl -fsSL https://repo.jellyfin.org/jellyfin_team.gpg.key \
        | gpg --dearmor -o /etc/apt/keyrings/jellyfin.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/jellyfin.gpg] https://repo.jellyfin.org/debian bookworm main" \
        > /etc/apt/sources.list.d/jellyfin.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends jellyfin-ffmpeg7 \
    && apt-get purge -y curl gnupg && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r plurx \
    && useradd -r -g plurx -d /var/lib/plurx plurx \
    && mkdir -p /var/lib/plurx \
    && chown plurx:plurx /var/lib/plurx
COPY --from=build /plurxd /usr/local/bin/plurxd

# Default to jellyfin-ffmpeg (recent GPUs need its driver stack); override
# either var to point elsewhere. It's a superset of system ffmpeg, so this is
# safe on hardware that the distro build would also handle.
ENV PLURX_BIND=0.0.0.0:32600 \
    PLURX_DATA_DIR=/var/lib/plurx \
    PLURX_FFMPEG=/usr/lib/jellyfin-ffmpeg/ffmpeg \
    PLURX_FFPROBE=/usr/lib/jellyfin-ffmpeg/ffprobe

EXPOSE 32600
VOLUME ["/var/lib/plurx"]
USER plurx

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD ["plurxd", "healthcheck"]

ENTRYPOINT ["plurxd"]
CMD ["run"]
