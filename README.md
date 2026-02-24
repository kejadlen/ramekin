# ramekin

A containerized setup for running the [pi coding agent](https://github.com/badlogic/pi-mono) with network-restricted access.

## Architecture

```
┌──────────────────────────────────────────────────┐
│            sidecar network namespace             │
│                                                  │
│  ┌─────────────┐        ┌─────────────────────┐  │
│  │    agent    │─:8080─▶│       sidecar       │  │
│  │    (pi)     │        │  ┌───────────────┐  │──┼──▶ api.anthropic.com:443
│  │             ├────────┼──┤ bridge server │  │  │
│  └─────────────┘        │  └───────────────┘  │  │
│                         │  iptables firewall  │  │
│                         └─────────────────────┘  │
└──────────────────────────────────────────────────┘
```

Two containers share a network namespace:

- **agent** — runs the [pi coding agent](https://github.com/badlogic/pi-mono) (`@mariozechner/pi-coding-agent`). Pi handles all Anthropic API communication internally.
- **sidecar** — runs iptables rules that restrict all outbound traffic to `api.anthropic.com:443`, plus a bridge HTTP server for controlled proxying.

Because the agent uses `network_mode: "service:sidecar"`, all of its traffic is subject to the sidecar's iptables rules.

## Prerequisites

- Docker and Docker Compose

## Usage

```sh
docker compose up --build
```

The agent container runs `pi` interactively with `stdin_open` and `tty` enabled. To attach to the agent and interact with pi:

```sh
docker compose attach agent
```

## Project structure

```
├── Cargo.toml              # Workspace root (sidecar only)
├── Dockerfile              # Agent container image (Node.js + pi)
├── docker-compose.yml      # Orchestration for both containers
└── sidecar/
    ├── Cargo.toml          # Sidecar crate (ramekin-sidecar)
    ├── Dockerfile          # Sidecar container image
    ├── entrypoint.sh       # iptables setup, then starts bridge
    └── src/
        └── main.rs         # Bridge HTTP server (axum)
```

## Network restrictions

The sidecar's `entrypoint.sh` configures iptables at startup:

1. Default policy is `DROP` for both `INPUT` and `OUTPUT`.
2. Loopback and DNS are allowed (so the bridge server and hostname resolution work).
3. Outbound HTTPS is allowed only to the resolved IPs of `api.anthropic.com`.
4. Inbound connections are accepted on the bridge port (`:8080`) so the agent can reach the bridge server.
5. Established/related return traffic is allowed through.

## Bridge server

The bridge server (`/echo` endpoint) accepts a JSON body and returns it unchanged. This provides a simple mechanism for the agent to verify connectivity to the sidecar.
