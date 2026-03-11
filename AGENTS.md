# Agents

## Project

Ramekin is a containerized harness for running the [pi coding agent](https://github.com/badlogic/pi-mono) with network-restricted access. A Rust CLI orchestrates two Docker containers that share a network namespace: an **agent** container running pi and a **firewall** container enforcing nftables rules that restrict all outbound traffic to `api.anthropic.com:443`.

## Repository layout

```
Cargo.toml              # Workspace root + ramekin CLI crate
src/main.rs             # CLI: generates compose config, starts containers, attaches to pi
Dockerfile              # Agent container (Node.js + pi)
firewall/
  Cargo.toml            # ramekin-firewall crate (bridge server)
  Dockerfile            # Firewall container (Rust binary + nftables)
  entrypoint.sh         # Resolves Anthropic IPs, loads nftables, starts bridge
  src/main.rs           # Axum bridge server (/echo endpoint)
justfile                # Local dev tasks (check, fmt, clippy, cov)
.github/workflows/      # CI (fmt + check + clippy + test) and CalVer release
```

## Build and test

```sh
cargo check --workspace   # Type-check both crates
cargo fmt --all           # Format all code
cargo clippy --workspace  # Lint
cargo test --workspace    # Run tests (firewall crate has tests; CLI does not yet)
```

Or use `just` which runs check, fmt, and clippy together:

```sh
just
```

## Conventions

- **Rust edition 2024**, resolver v2 workspace.
- Error handling uses `color-eyre`. Prefer `wrap_err` / `bail!` over `.unwrap()`.
- Logging uses `tracing` with `tracing-subscriber` and `EnvFilter`. Use `tracing::info`, `tracing::error`, etc. — not `println!` or `eprintln!`.
- All CI checks must pass: `cargo fmt --all --check`, `cargo clippy --workspace`, `cargo test --workspace`.
- The firewall's `entrypoint.sh` must not `flush ruleset` — it deletes and recreates only `table inet filter` to preserve Docker's iptables-nft NAT/DNS rules.

## Architecture notes

- Docker compose config is generated at runtime, not a static file. The `generate_compose` function builds YAML based on firewall flag, Dockerfile path, and volume mounts.
- With firewall enabled (default), the agent uses `network_mode: "service:firewall"` so all its traffic traverses the firewall's nftables rules. With `--no-firewall`, the agent runs with normal Docker networking.
- XDG directories under the `ramekin` prefix store pi state: data in `XDG_DATA_HOME/ramekin`, config in `XDG_CONFIG_HOME/ramekin`.
- BYOC: if the workspace contains `.ramekin/Dockerfile`, the CLI uses it as the agent image instead of the default.
- The bridge server currently only has an `/echo` endpoint. It is not a proxy.

## Task management

Tasks are tracked with `ranger` (backlog name: `ramekin`). Use the `ranger` skill for commands and workflow. Pick up the next queued task from the top of the queue unless directed otherwise.
