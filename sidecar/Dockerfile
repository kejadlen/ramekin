FROM golang:1.23-bookworm AS builder

WORKDIR /app
COPY go.mod ./
COPY bridge.go ./

RUN CGO_ENABLED=0 go build -o bridge .

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    iptables \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/bridge /usr/local/bin/bridge
COPY entrypoint.sh /usr/local/bin/entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["entrypoint.sh"]
