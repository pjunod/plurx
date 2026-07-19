# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p plurxd && cp target/release/plurxd /plurxd

FROM debian:bookworm-slim
RUN groupadd -r plurx \
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
