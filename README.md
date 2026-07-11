# Ramekin

Containerized harness for running the [pi coding agent](https://github.com/badlogic/pi-mono).

## Quick start

```sh
ramekin              # run pi against the current directory
ramekin /some/path   # mount a specific workspace
ramekin run --rebuild  # force a full image rebuild
```

Ramekin builds a Docker image with pi and its dependencies, starts it via Docker Compose, and attaches your terminal. Auth tokens and session history persist across runs.

## How it works

A Rust CLI orchestrates a Docker Compose stack. On each run it:

1. Creates XDG directories for persistent state
2. Writes the embedded Dockerfile to `$XDG_CACHE_HOME/ramekin/`
3. Generates a compose config, renders the system prompt, and creates a fresh agent dir, all in a session-scoped cache directory
4. Builds the agent image (and a project-specific layer, if one exists)
5. Starts the agent container with the workspace mounted at `/workspace/<slug>` (where `<slug>` is `<dirname>-<hash>`, so cwd-keyed agent state never collides across repos)
6. Attaches interactively, then tears down on exit — logging any state the agent wrote to its session-scoped dir before discarding it

Concurrent sessions don't interfere: everything a run touches is either read-only config, session-scoped plumbing under a random session id, or agent state the agent itself manages concurrently.

### Subcommands

`run` (default) starts a containerized pi session. Pass `--rebuild` to ignore Docker layer cache and pull fresh base images.

`config` prints resolved paths, volume mounts, and Dockerfile status without starting anything — useful for debugging mount issues.

`completions <shell>` generates shell completions for bash, zsh, fish, elvish, or powershell. Pipe the output to a file sourced by your shell:

```sh
ramekin completions zsh > ~/.zfunc/_ramekin
ramekin completions bash > ~/.local/share/bash-completion/completions/ramekin
```

### Agent config

Pi's config comes from the host's own agent dir: the config-shaped entries of `~/.pi/agent/` (`AGENTS.md`, `skills/`) mount read-only at their normal paths inside the container. Ramekin keeps no parallel copy — edit the host files (or the dotfiles they symlink to) and the next session sees the changes. The rest of `~/.pi/agent/` is host runtime state (credentials, session history) and never enters the container.

Config is immutable from inside the container by design: in-container edits to it fail loudly instead of silently disappearing.

### Persistence

Pi's container state is ephemeral by default: each session gets a fresh, empty writable dir at `/root/.pi/agent`, discarded on teardown. What persists is allowlisted and bind-mounted on top:

- `auth.json` — global, at `$XDG_DATA_HOME/ramekin/agents/pi/auth.json`, so the containerized agent keeps its own credentials (separate from the host's)
- `sessions/` — per-repo, at `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/`

On teardown, ramekin logs anything else the agent wrote to its session dir before discarding it, so a path that deserves persistence gets noticed rather than silently vanishing.

### Volume mounts

Mount configuration merges across three layers, lowest to highest precedence:

1. **Binary** — compiled-in staples (`~/.config/git`, `~/.config/jj`, read-only, skipped when missing) and the pi agent-config mounts described above
2. **User** — `$XDG_CONFIG_HOME/ramekin/config.kdl`
3. **Project** — `<workspace>/.ramekin/config.kdl`

Additional host paths can be mounted into the container via the KDL layers. Directories, files, and devices (such as `/dev/null`) all work. Mounts whose source doesn't exist on the host are silently skipped.

**User config** — `$XDG_CONFIG_HOME/ramekin/config.kdl`

```kdl
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
| `target` | no | Container path; `~` expands to the container home, a relative path resolves against the workspace mount, and omitting it derives the target from the source |
| `writable` | no | Allow writes (read-only by default) |

When two layers define a mount with the same container target, the higher layer wins wholesale. A `/dev/null` source *masks*: it removes a mount inherited from a lower layer (say, a staple or an agent-config entry this machine or project doesn't want), and where there's nothing to remove it stays a real bind, blanking a workspace file from the agent:

```kdl
// Remove the inherited skills mount
mounts {
    source "/dev/null"
    target "/root/.pi/agent/skills"
}

// Hide the repo's .envrc from the agent
mounts {
    source "/dev/null"
    target ".envrc"
}
```

Session mounts (the workspace, the agent dir plumbing, `auth.json`, `sessions/`, the rendered prompt) are forced and cannot be overridden from config.

### Container environment context

A built-in system prompt (`ramekin-prompt.md`) is rendered per session, mounted read-only into the agent dir, and passed to pi via `--append-system-prompt`. It tells the agent about the container environment — the workspace mount, ephemeral filesystem, read-only config, and networking. AGENTS.md remains fully available for user customization.

### Custom Dockerfile

Place a `Dockerfile` at `.ramekin/Dockerfile` in your workspace to extend the base agent image. Use `FROM ramekin-agent` to layer on top — the base image includes Node.js, pi, git, jj, ripgrep, fd, just, jq, difftastic, ranger, and Rust tooling.

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
