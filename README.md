# Ramekin

Containerized harness for running [pi](https://github.com/badlogic/pi-mono) or [Claude Code](https://github.com/anthropics/claude-code) inside Docker.

## Quick start

```sh
ramekin              # run the configured agent against the current directory
ramekin /some/path   # mount a specific workspace
ramekin run --rebuild  # force a full image rebuild
```

Ramekin builds a Docker image with the agent and its dependencies, starts it via Docker Compose, and attaches your terminal. Auth tokens and session history persist across runs. Pi is the default agent; set `agent "claude"` in your KDL config to run Claude Code instead.

## How it works

A Rust CLI orchestrates a Docker Compose stack. On each run it:

1. Creates XDG directories for persistent state
2. Writes the embedded Dockerfile to `$XDG_CACHE_HOME/ramekin/`
3. Generates a compose config and writes it to a session-scoped cache directory
4. Builds the agent image (and a project-specific layer, if one exists)
5. Starts the agent container with the workspace mounted at `/workspace`
6. Attaches interactively, then tears down on exit

### Subcommands

`run` (default) starts a containerized agent session. Pass `--rebuild` to ignore Docker layer cache and pull fresh base images. Pass extra agent flags after `--`, for example `ramekin -- --model sonnet-4.6`.

`config` prints resolved paths, volume mounts, and Dockerfile status without starting anything — useful for debugging mount issues.

`completions <shell>` generates shell completions for bash, zsh, fish, elvish, or powershell. Pipe the output to a file sourced by your shell:

```sh
ramekin completions zsh > ~/.zfunc/_ramekin
ramekin completions bash > ~/.local/share/bash-completion/completions/ramekin
```

### Selecting an agent

The active agent is set in KDL via the top-level `agent` field. Defaults to `"pi"` for back-compatibility.

```kdl
// User config — ~/.config/ramekin/config.kdl
agent "claude"
```

Project-scope KDL (`<workspace>/.ramekin/config.kdl`) can override the user setting per-repo.

### Persistence

#### Pi

- Agent dir (`$XDG_CONFIG_HOME/ramekin/agent/`) mounts at `/root/.pi/agent`. On each run everything except `auth.json` is cleared, then files declared in the KDL `pi { ... }` block are copied in.
- Pi data dir (`$XDG_DATA_HOME/ramekin/`) mounts at `/root/.pi` for auth and session history.
- Each workspace gets a per-repo sessions directory at `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/`, mounted at `/root/.pi/agent/sessions`.

#### Claude

- Claude data dir (`$XDG_DATA_HOME/ramekin/agents/claude/`) mounts at `/root/.claude` for settings, auth, history, and runtime state.
- Each workspace gets a per-repo projects directory at `$XDG_DATA_HOME/ramekin/repos/<slug>/claude-projects/`, mounted at `/root/.claude/projects/-workspace`. Claude Code keys session history off the cwd; the workspace always mounts at `/workspace`, so the encoded path is always `-workspace`. Without the per-repo split every host repo would share the same history.
- Each workspace also gets a per-repo `.claude.json` at `$XDG_DATA_HOME/ramekin/repos/<slug>/claude.json`, mounted at `/root/.claude.json`. Claude Code's global state file has a `projects` map keyed by absolute cwd; without the split, granted permissions and prompt history would leak across host repos.

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

Mounts are merged across scopes. When both user and project configs define a mount with the same container target, the project mount wins. Builtin mounts (workspace, agent state) cannot be overridden.

### Agent config injection

Each agent has its own KDL block for declaring host paths to expose inside the container. The two blocks differ in mechanics, reflecting how each agent expects its config dir to behave.

`pi { ... }` copies entries into `/root/.pi/agent/` on every run. The agent dir is wiped (preserving `auth.json`) and reassembled from scratch, so removing an entry from KDL drops the file on the next run.

```kdl
pi {
    source "~/.dotfiles/ai/AGENTS.md"
}
pi {
    source "~/.dotfiles/ai/skills"
}
```

`claude { ... }` mounts entries as Docker bind mounts at `/root/.claude/<target>`. Host-side edits stay live in the container, and the underlying claude data dir keeps auth and runtime state untouched. Read-only by default.

```kdl
claude {
    source "~/.dotfiles/ai/claude/CLAUDE.md"
}
claude {
    source "~/.dotfiles/ai/claude/skills"
}
claude {
    source "~/.dotfiles/ai/claude/settings.json"
    writable
}
```

Each block supports the same fields:

| Field | Required | Description |
|---|---|---|
| `source` | yes | Host path (`~` expands to home directory) |
| `target` | no | Path inside the agent dir (derived from source basename if omitted) |
| `writable` | no | Allow writes (claude only; read-only by default) |

### Container environment extension

A built-in pi extension (`ramekin.ts`) is written into the agent container when running pi. It appends container environment context to the system prompt via `before_agent_start`, telling the agent about the workspace mount, ephemeral filesystem, and networking. For Claude Code the same prompt content lands at `/root/.claude/ramekin-prompt.md` and ramekin passes it via `--append-system-prompt`. AGENTS.md (or CLAUDE.md) remains fully available for user customization.

### Custom Dockerfile

Place a `Dockerfile` at `.ramekin/Dockerfile` in your workspace to extend the base agent image. Use `FROM ramekin-agent` to layer on top — the base image includes Node.js, the selected agent (pi or Claude Code), git, jj, ripgrep, fd, just, jq, difftastic, ranger, and Rust tooling.

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
