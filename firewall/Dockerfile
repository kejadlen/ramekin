FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY firewall/ firewall/

RUN cargo build --release --bin ramekin-firewall

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    iptables \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ramekin-firewall /usr/local/bin/bridge
COPY firewall/entrypoint.sh /usr/local/bin/entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["entrypoint.sh"]
