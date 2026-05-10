# Build romanz/electrs from source for native arm64/amd64.
#
# bp-wallet 0.11.1-alpha.2's electrum-rpc parser is tightly coupled to romanz/electrs's
# `blockchain.transaction.get` response shape; blockstream-fork derivatives
# (mempool/electrs, getumbrel/electrs) silently fail to parse one of the response fields,
# the tx never lands in the cache, and bp-wallet then trips its
# `expect("broken logic")` at src/indexers/electrum.rs:220.
#
# Mirrors romanz/electrs's upstream Dockerfile (v0.11.1) but clones the source inside
# the build stage so we don't carry a submodule or vendored copy. Override the ref via
# `ELECTRS_REF` build arg if you need to pin a different commit/tag.

ARG ELECTRS_REF=v0.11.1

FROM debian:trixie-slim AS base
RUN apt-get update -qqy \
 && apt-get install -qqy --no-install-recommends librocksdb-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*

FROM base AS builder
ARG ELECTRS_REF
RUN apt-get update -qqy \
 && apt-get install -qqy --no-install-recommends git cargo build-essential libclang-dev \
 && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "${ELECTRS_REF}" https://github.com/romanz/electrs.git /src
WORKDIR /src
ENV ROCKSDB_INCLUDE_DIR=/usr/include \
    ROCKSDB_LIB_DIR=/usr/lib
RUN cargo install --locked --path . --root /out

FROM base
COPY --from=builder /out/bin/electrs /usr/local/bin/electrs
ENTRYPOINT ["electrs"]
