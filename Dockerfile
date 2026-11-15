# syntax=docker/dockerfile:1
FROM rust:1.89.0-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY service ./service
RUN cargo build --release --locked --manifest-path service/Cargo.toml \
    && mkdir /data && chown 10001:10001 /data

FROM scratch
ARG VCS_REF=unknown
ARG SOURCE_URL=https://github.com/shrunk-db/shrunk
LABEL org.opencontainers.image.title="shrunk" \
      org.opencontainers.image.description="Authenticated HTTP time series database with queryable compression" \
      org.opencontainers.image.source="${SOURCE_URL}" \
      org.opencontainers.image.licenses="GPL-3.0-or-later" \
      org.opencontainers.image.revision="${VCS_REF}"
COPY --from=build /build/service/target/release/shrunk-server /usr/local/bin/shrunk-server
COPY --from=build --chown=10001:10001 /data /data
COPY LICENSE /LICENSE
USER 10001:10001
ENV SHRUNK_LISTEN=0.0.0.0:8080 SHRUNK_DATA_DIR=/data
EXPOSE 8080
VOLUME ["/data"]
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/shrunk-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/shrunk-server"]
