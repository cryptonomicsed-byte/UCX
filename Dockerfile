FROM rust:1-slim-bookworm AS builder
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY . .
RUN cargo build --release --package ucx-broker

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 ucx

COPY --from=builder /build/target/release/ucx-broker /usr/local/bin/ucx-broker

USER ucx
EXPOSE 7790
ENTRYPOINT ["/usr/local/bin/ucx-broker"]
