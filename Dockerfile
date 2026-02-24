FROM rust:1.85-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY sidecar/Cargo.toml sidecar/Cargo.toml
RUN mkdir -p sidecar/src && echo "fn main() {}" > sidecar/src/main.rs

RUN cargo build --release --bin ramekin

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/ramekin /usr/local/bin/ramekin

ENV RUST_LOG=info

ENTRYPOINT ["ramekin"]
