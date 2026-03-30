FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/
ENV RUSTFLAGS="-C target-cpu=native"
RUN cargo build --release --bin logfwd

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 logfwd \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /sbin/nologin logfwd
COPY --from=builder /src/target/release/logfwd /usr/local/bin/logfwd
# Diagnostics / health endpoint
EXPOSE 9090
USER logfwd
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:9090/health || exit 1
ENTRYPOINT ["logfwd"]
