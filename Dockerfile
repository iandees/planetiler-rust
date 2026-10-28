FROM rust:1-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# reported by `planetiler --version` and in the startup log
ARG PLANETILER_GITHASH
ARG PLANETILER_BUILD_TIMESTAMP
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p planetiler \
    && cp target/release/planetiler /planetiler

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /planetiler /usr/local/bin/planetiler
# like the Java image: run from / so the default ./data directory is the /data volume
VOLUME /data
ENTRYPOINT ["planetiler"]
