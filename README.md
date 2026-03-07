# Ramekin

Containerized harness for running the [pi coding agent](https://github.com/badlogic/pi-mono) with network-restricted access.

## Quick start

```sh
cargo run               # run pi in a container against the current directory
cargo run -- /some/path  # mount a specific workspace
```

Ramekin builds a Docker image with pi and its dependencies, starts it via Docker Compose, and attaches your terminal. Auth state, settings, and keybindings persist across runs.

## How it works

A Rust CLI orchestrates a Docker Compose stack. On each run it:

1. Creates XDG directories for persistent state
2. Writes the embedded Dockerfile and compose file to `$XDG_CACHE_HOME/ramekin/`
3. Starts the agent container with the workspace mounted at `/workspace`
4. Attaches interactively, then tears down on exit

### Persistence

| What | Where (host) | Where (container) |
|---|---|---|
| Auth, sessions | `$XDG_DATA_HOME/ramekin/` | `/root/.pi` |
| settings.json | `$XDG_CONFIG_HOME/ramekin/settings.json` | `/root/.pi/agent/settings.json` |
| keybindings.json | `$XDG_CONFIG_HOME/ramekin/keybindings.json` | `/root/.pi/agent/keybindings.json` |
| AGENTS.md | `$XDG_CONFIG_HOME/ramekin/AGENTS.md` | `/root/.pi/agent/AGENTS.md` |

Settings and keybindings are seeded as empty JSON (`{}`) on first run. AGENTS.md is seeded empty.

### Container environment extension

A built-in pi extension (`ramekin.ts`) is mounted into the agent container. It appends container environment context to the system prompt via `before_agent_start`, telling the agent about the workspace mount, ephemeral filesystem, and networking constraints (when the firewall is enabled). AGENTS.md remains fully available for user customization.

### Custom Dockerfile

Place a `Dockerfile` at `.ramekin/Dockerfile` in your workspace to override the default agent image. The workspace is used as the build context, so `COPY` instructions work relative to the project root.

## Development

```sh
cargo check    # type-check
cargo fmt      # format
cargo clippy   # lint
just           # all three
```
