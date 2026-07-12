# Ramekin

Containerized harness for running coding agents: the [pi coding agent](https://github.com/badlogic/pi-mono) or [Claude Code](https://github.com/anthropics/claude-code).

## Quick start

```sh
ramekin                # run the default profile (pi) against the current directory
ramekin -p claude      # run Claude Code instead
ramekin /some/path     # mount a specific workspace
ramekin run --rebuild  # force a full image rebuild
ramekin -- --model X   # forward extra args to the agent
```

Ramekin builds a Docker image with the agent and its dependencies, starts it via Docker Compose, and attaches your terminal. Auth tokens and session history persist across runs.

## How it works

A Rust CLI orchestrates a Docker Compose stack. On each run it:

1. Resolves the active profile (which picks the agent) and merges config layers
2. Builds the agent's base image (`ramekin-pi` or `ramekin-claude`), and a project-specific layer if one exists
3. Generates a compose config, renders the system prompt, and creates fresh agent dirs, all in a session-scoped cache directory
4. Starts the agent container with the workspace mounted at `/workspace/<slug>` (where `<slug>` is `<dirname>-<hash>`, so cwd-keyed agent state never collides across repos)
5. Attaches interactively, then tears down on exit — logging any state the agent wrote to its session-scoped dirs before discarding it, and keeping any config proposals the agent left in its outbox

Concurrent sessions don't interfere: everything a run touches is either read-only config, session-scoped plumbing under a random session id, or agent state the agent itself manages concurrently.

### Subcommands

`run` (default) starts a containerized agent session. Pass `--rebuild` to ignore Docker layer cache and pull fresh base images, `-p <profile>` to pick a profile for this run.

`config` prints the active profile, resolved paths, volume mounts, and Dockerfile status without starting (or mutating) anything — useful for debugging mount issues.

`outbox` reviews config changes proposed by agents — see [Outbox](#outbox).

`completions <shell>` generates shell completions for bash, zsh, fish, elvish, or powershell. Pipe the output to a file sourced by your shell:

```sh
ramekin completions zsh > ~/.zfunc/_ramekin
ramekin completions bash > ~/.local/share/bash-completion/completions/ramekin
```

### Profiles

A profile is a named bundle of agent + provider plumbing: which agent to run, env vars (with host passthrough for credentials), and extra mounts. The binary ships two trivial profiles — `pi` and `claude`, bare agents with no provider plumbing — so ramekin runs with zero config. Everything richer is defined in KDL:

```kdl
// e.g. ~/.config/ramekin/profiles.kdl — symlinked from dotfiles, shared
profile "claude-bedrock" {
    agent "claude"
    env {
        CLAUDE_CODE_USE_BEDROCK "1"
        AWS_PROFILE              // bare = pass the host's value through
    }
    mounts { source "~/.aws" }
}

profile "pi-glm" {
    agent "pi"
    env {
        ANTHROPIC_BASE_URL "https://open.bigmodel.cn/api/anthropic"
        ZHIPU_API_KEY
    }
}
```

A bare `profile "name"` node (no block) *selects* a profile. Selection precedence, lowest to highest: binary default (`pi`) → user KDL (the per-machine default) → project KDL → project-local KDL → `-p` on the command line. Profile selection subsumes agent selection; there is no separate `--agent`. Model choice within a provider stays out of profiles — that's per-run agent args after `--`.

Profiles merge by name across layers and the last writer takes the whole definition. Fine-grained tweaks (one env var) go through the ordinary layered `env`, which overlays the active profile's env per variable.

Provider credentials never appear in config: passthrough env forwards host values at run time, mounts carry their own files, and OAuth lives in the container's persistent agent state.

### Agent config

Agent config comes from the host's own dirs — the agents are also used locally, so their config already exists where they look for it. The config-shaped entries mount read-only at their normal paths inside the container:

- pi: `~/.pi/agent/` — `AGENTS.md`, `skills/`
- claude: `~/.claude/` — `CLAUDE.md`, `settings.json`, `skills/`, `agents/`, `commands/`

Ramekin keeps no parallel copy — edit the host files (or the dotfiles they symlink to) and the next session sees the changes. The rest of each host dir is runtime state (credentials, transcripts) and never enters the container. Project-level agent config (`.claude/`, `CLAUDE.md`, `AGENTS.md` in the repo) rides the workspace mount; the agents layer it themselves.

Config is immutable from inside the container by design: in-container edits fail loudly, and the [outbox](#outbox) is the write path.

### Persistence

The two agents get opposite persistence policies, chosen by failure mode.

**Pi: ephemeral by default, allowlist what persists.** Each session gets a fresh, empty writable dir at `/root/.pi/agent`, discarded on teardown, with the persistent pieces bound on top:

- `auth.json` — global, at `$XDG_DATA_HOME/ramekin/agents/pi/auth.json`, so the containerized agent keeps its own credentials (separate from the host's)
- `sessions/` — per-repo, at `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/`

On teardown, ramekin logs anything else the agent wrote to its session dir before discarding it, so a path that deserves persistence gets noticed rather than silently vanishing.

**Claude: persist by default, denylist the junk.** `~/.claude` and `~/.claude.json` mount from `$XDG_DATA_HOME/ramekin/agents/`, shared across repos — auth, identity, onboarding state, and transcripts all survive. Fresh session-scoped dirs bind over the known ephemeral subdirs (`statsig/`, `todos/`, `shell-snapshots/`, `debug/`). Per-repo isolation of Claude's cwd-keyed `projects` map comes from the `/workspace/<slug>` mount, not from splitting the state file. If Claude grows an unclassified state file, it persists (worst case: rot) rather than vanishing (worst case: lost auth).

### Volume mounts

Mount configuration merges across layers, lowest to highest precedence:

1. **Binary** — compiled-in staples (`~/.config/git`, `~/.config/jj`, read-only, skipped when missing) and the agent-config mounts described above
2. **Profile** — the active profile's `mounts`
3. **User** — every `*.kdl` in `$XDG_CONFIG_HOME/ramekin/`, merged as one layer (defining the same key twice within the layer is an error; per-file symlinking into dotfiles is a dotfiles decision)
4. **Project** — `<workspace>/.ramekin/config.kdl`, committed
5. **Project-local** — `<workspace>/.ramekin/config.local.kdl`, gitignored

Additional host paths can be mounted into the container via the KDL layers. Directories, files, and devices (such as `/dev/null`) all work. Mounts whose source doesn't exist on the host are silently skipped.

```kdl
// Mount ranger database (writable)
mounts {
    source "~/.local/share/ranger"
    writable
}

// Mount extra data at an explicit path
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

Session mounts (the workspace, agent state, the rendered prompt, the outbox) are forced and cannot be overridden from config.

### Environment variables

`env` has exactly one syntax: a block with one child node per variable. The single argument is the value; omit it to pass the host's value through at run time (the value never lands in config or the generated compose file). `env` merges per variable across layers, overlaying the active profile's env.

```kdl
env {
    RUST_BACKTRACE "1"
    GITHUB_TOKEN            // forwarded from the host environment
}
```

### Outbox

Shared config is read-only in the container, so the outbox is the one reviewed path for changing it. Each session mounts a fresh, empty dir at `/root/.ramekin/outbox` (host: `$XDG_DATA_HOME/ramekin/repos/<slug>/outbox/<session-id>/`), and the system prompt tells the agent to write complete updated files there, mirroring the agent config layout. Empty outboxes vanish at teardown; anything left becomes a pending proposal.

```sh
ramekin outbox list                          # pending proposals across sessions
ramekin outbox diff [<slug>/<session>/<path>] # diff against the host source
ramekin outbox apply <slug>/<session>/<path>  # copy over the host source, after confirmation
ramekin outbox discard <slug>/<session>       # drop proposals
```

`apply` shows the diff, asks for confirmation, and writes through dotfiles symlinks so the change lands in the working copy. Proposals that don't map back to an allowlisted agent-config entry need an explicit `--to` destination.

### Container environment context

A built-in system prompt (`ramekin-prompt.md`) is rendered per session, mounted read-only at `/root/.ramekin/ramekin-prompt.md`, and passed to the agent (`--append-system-prompt` for pi, `--append-system-prompt-file` for Claude). It tells the agent about the container environment — the workspace mount, ephemeral filesystem, read-only config, the outbox, and networking. `AGENTS.md`/`CLAUDE.md` remain fully available for user customization.

For Claude, the base image bakes in yolo mode: managed settings set `permissions.defaultMode = bypassPermissions` and `IS_SANDBOX=1` acknowledges the container as the sandbox.

### Custom Dockerfile

Place a `Dockerfile` at `.ramekin/Dockerfile` in your workspace to extend the base agent image. Declare `ARG BASE` / `FROM ${BASE}` — ramekin passes the active agent's base tag, so one project Dockerfile serves both agents. The base images include Node.js, the agent, git, jj, ripgrep, fd, just, jq, difftastic, dotslash, and ranger.

The workspace is used as the build context, so `COPY` instructions work relative to the project root.

```dockerfile
ARG BASE
FROM ${BASE}
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
