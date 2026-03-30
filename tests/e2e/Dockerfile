FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/
# Disable LTO and use multiple codegen units to avoid OOM during linking.
# The e2e test doesn't need production-grade optimization.
RUN CARGO_PROFILE_RELEASE_LTO=off \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    CARGO_PROFILE_RELEASE_DEBUG=false \
    cargo build --release --bin logfwd

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/logfwd /usr/local/bin/logfwd
ENTRYPOINT ["logfwd"]
