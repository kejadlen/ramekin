# Ramekin

Containerized harness for running the [pi coding agent](https://github.com/badlogic/pi-mono) with network-restricted access.

## Quick start

```sh
ramekin              # run pi against the current directory
ramekin /some/path   # mount a specific workspace
ramekin run --rebuild  # force a full image rebuild
```

Ramekin builds a Docker image with pi and its dependencies, starts it via Docker Compose, and attaches your terminal. Auth state, settings, and keybindings persist across runs.

## How it works

A Rust CLI orchestrates a Docker Compose stack. On each run it:

1. Creates XDG directories for persistent state
2. Writes the embedded Dockerfile to `$XDG_CACHE_HOME/ramekin/`
3. Generates a compose config and writes it to a session-scoped cache directory
4. Builds the agent image (and a project-specific layer, if one exists)
5. Starts the agent container with the workspace mounted at `/workspace`
6. Attaches interactively, then tears down on exit

### Subcommands

`run` (default) starts a containerized pi session. Pass `--rebuild` to ignore Docker layer cache and pull fresh base images.

`config` prints resolved paths, volume mounts, and Dockerfile status without starting anything — useful for debugging mount issues.

`completions <shell>` generates shell completions for bash, zsh, fish, elvish, or powershell. Pipe the output to a file sourced by your shell:

```sh
ramekin completions zsh > ~/.zfunc/_ramekin
ramekin completions bash > ~/.local/share/bash-completion/completions/ramekin
```

### Persistence

The agent directory (`$XDG_CONFIG_HOME/ramekin/agent/`) is mounted into the container at `/root/.pi/agent`. It holds:

| File | Seeded as |
|---|---|
| settings.json | `{}` |
| keybindings.json | `{}` |
| AGENTS.md | empty |

The full pi data directory (`$XDG_DATA_HOME/ramekin/`) is mounted at `/root/.pi` for auth tokens and session history. Each workspace also gets its own sessions directory under `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/`.

### Volume mounts

Additional host directories can be mounted into the container via KDL config files. Mounts whose source doesn't exist on the host are silently skipped.

**User config** — `$XDG_CONFIG_HOME/ramekin/config.kdl`

```kdl
// Mount git and jj config (read-only by default)
mounts {
    source "~/.config/git"
}
mounts {
    source "~/.config/jj"
}

// Mount ranger database (writable)
mounts {
    source "~/.local/share/ranger"
    writable
}
```

**Project config** — `<workspace>/.ramekin/config.kdl`

```kdl
// Mount extra data into the container
mounts {
    source "~/datasets"
    target "/root/datasets"
}
```

Each `mounts` block supports:

| Field | Required | Description |
|---|---|---|
| `source` | yes | Host path (`~` expands to home directory) |
| `target` | no | Container path (derived from source if omitted) |
| `writable` | no | Allow writes (read-only by default) |

Mounts are merged across scopes. When both user and project configs define a mount with the same container target, the project mount wins. Builtin mounts (workspace, pi data, agent dir) cannot be overridden.

### Container environment extension

A built-in pi extension (`ramekin.ts`) is mounted into the agent container. It appends container environment context to the system prompt via `before_agent_start`, telling the agent about the workspace mount, ephemeral filesystem, and networking. AGENTS.md remains fully available for user customization.

### Custom Dockerfile

Place a `Dockerfile` at `.ramekin/Dockerfile` in your workspace to extend the base agent image. Use `FROM ramekin-agent` to layer on top — the base image includes Node.js, pi, git, jj, ripgrep, fd, just, and Rust tooling.

The workspace is used as the build context, so `COPY` instructions work relative to the project root.

```dockerfile
FROM ramekin-agent
RUN apt-get update && apt-get install -y ruby && rm -rf /var/lib/apt/lists/*
```

## Development

```sh
cargo check    # type-check
cargo fmt      # format
cargo clippy   # lint
cargo test     # run tests
just           # all four
just install   # cargo install from local source
```
