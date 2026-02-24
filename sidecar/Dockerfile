FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY sidecar/Cargo.toml sidecar/Cargo.toml
COPY sidecar/src/ sidecar/src/
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

RUN cargo build --release --bin ramekin-sidecar

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    iptables \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ramekin-sidecar /usr/local/bin/bridge
COPY sidecar/entrypoint.sh /usr/local/bin/entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["entrypoint.sh"]
