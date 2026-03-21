# Agents

## Project

Ramekin is a containerized harness for running the [pi coding agent](https://github.com/badlogic/pi-mono). A Rust CLI builds a Docker image, generates a compose config at runtime, and attaches the user's terminal to the agent container. Network restriction via a firewall sidecar is planned but not yet implemented.

## Repository layout

```
Cargo.toml              # Single-crate workspace
src/main.rs             # CLI: builds image, generates compose, starts container, attaches
build.rs                # Sets RAMEKIN_VERSION from env or git rev
assets/
  Dockerfile            # Agent container image (Node.js + pi + jj + Rust)
  ramekin.ts            # Pi extension injected into the container
clippy.toml             # Disallows std::fs in favor of fs-err
justfile                # Local dev tasks (check, fmt, clippy, test, install)
.github/workflows/
  ci.yml                # CI: fmt + clippy + test
  release.yml           # CalVer release: build macOS binary + DotSlash
```

## Build and test

```sh
cargo check    # Type-check
cargo fmt      # Format
cargo clippy   # Lint
cargo test     # Run tests
just           # All four
```

## Conventions

- Rust edition 2024, resolver v2 workspace.
- Error handling uses `color-eyre`. Prefer `wrap_err` / `bail!` over `.unwrap()`.
- File I/O uses `fs-err` instead of `std::fs`. The clippy config enforces this.
- Logging uses `tracing` with `tracing-subscriber` and `EnvFilter`. Use `tracing::info`, `tracing::error`, etc. — not `println!` or `eprintln!`.
- All CI checks must pass: `cargo fmt --all --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`.

## Architecture notes

- Docker compose config is generated at runtime, not a static file. The `generate_compose` function builds a YAML string from resolved paths and volume mounts.
- XDG directories under the `ramekin` prefix store pi state: data in `$XDG_DATA_HOME/ramekin`, config in `$XDG_CONFIG_HOME/ramekin`.
- Each workspace gets a per-repo sessions directory keyed by a `<dirname>-<hash>` slug.
- If the workspace contains `.ramekin/Dockerfile`, the CLI builds it on top of `ramekin-agent` instead of using the base image directly.
- The `ramekin.ts` extension is written into the agent config directory on every run. It injects container environment context into the pi system prompt.
- Version is set at build time via the `RAMEKIN_VERSION` env var (used by CI) or falls back to `dev+<short-sha>`.

## Task management

Tasks are tracked with `ranger` (backlog name: `ramekin`). Use the `ranger` skill for commands and workflow. Pick up the next queued task from the top of the queue unless directed otherwise.
