# Broker (rfq-api) image. Build context is the REPO ROOT (the whole workspace):
#   docker build -f infra/broker.Dockerfile -t rgb-rfq/broker .
# or via infra/signet/docker-compose.yml, which sets context: ../.. for you.
#
# reqwest is rustls (no OpenSSL); a C toolchain is still needed for secp256k1-sys
# and friends pulled in by the rgb/bp deps.

FROM rust:1.84-slim-bookworm AS build
WORKDIR /src
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config build-essential \
 && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p rfq-api

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/rfq-api /usr/local/bin/rfq-api

# Bind on all interfaces inside the container; compose maps the host port and
# supplies BROKER_DATABASE_URL / BROKER_ELECTRUM_URL.
ENV BROKER_LISTEN=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["rfq-api"]
