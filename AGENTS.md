# Agents

## Project

Ramekin is a containerized harness for running [pi](https://github.com/badlogic/pi-mono) or [Claude Code](https://github.com/anthropics/claude-code). A Rust CLI builds a Docker image, generates a compose config at runtime, and attaches the user's terminal to the agent container. Agent selection is per-config via the KDL `agent` field (`"pi"` default, `"claude"`). Network restriction via a firewall sidecar is planned but not yet implemented.

## Repository layout

```
Cargo.toml              # Single-crate workspace
src/
  main.rs               # CLI: AgentLayout, builds image, generates compose, attaches
  config.rs             # KDL loading, mount resolution, pi/claude entry assembly
build.rs                # Sets RAMEKIN_VERSION from env or git rev
assets/
  Dockerfile            # Pi container image (Node.js + pi + jj + Rust)
  Dockerfile.claude     # Claude Code container image (same base, claude-code instead of pi)
  ramekin-prompt.md     # System prompt appended inside the container
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
- Error handling uses `miette`. Prefer `wrap_err` / `bail!` over `.unwrap()`. Use `.into_diagnostic()` to convert standard errors before `?` or `.wrap_err()`.
- File I/O uses `fs-err` instead of `std::fs`. The clippy config enforces this.
- Logging uses `tracing` with `tracing-subscriber` and `EnvFilter`. Use `tracing::info`, `tracing::error`, etc. — not `println!` or `eprintln!`.
- All CI checks must pass: `cargo fmt --all --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`.

## Architecture notes

- Docker compose config is generated at runtime, not a static file. The `generate_compose` function builds a YAML string from resolved paths and volume mounts.
- `AgentLayout` (in `main.rs`) is an enum with `Pi` and `Claude` variants. Each variant carries the host paths, builtin mounts, embedded Dockerfile, and prompt path for that agent. `Ramekin::resolve` reads the effective agent from KDL, then builds the matching layout.
- XDG directories under the `ramekin` prefix store all agent state: data in `$XDG_DATA_HOME/ramekin`, config in `$XDG_CONFIG_HOME/ramekin`. Pi keeps its agent dir under `$XDG_CONFIG_HOME/ramekin/agent/`. Claude data lives at `$XDG_DATA_HOME/ramekin/agents/claude/`.
- Each workspace gets a per-repo state directory keyed by a `<dirname>-<hash>` slug at `$XDG_DATA_HOME/ramekin/repos/<slug>/`. Pi uses `sessions/`, claude uses `claude-projects/` and a `claude.json` file. The per-repo splits prevent cross-repo collisions on cwd-keyed paths inside `~/.claude/`.
- The `pi { ... }` block does copy-and-clear assembly: each run wipes the agent dir (preserving `auth.json`) and copies declared sources in. The `claude { ... }` block expands to Docker bind mounts at `/root/.claude/<target>`, so host edits stay live and runtime state is untouched.
- If the workspace contains `.ramekin/Dockerfile`, the CLI builds it on top of `ramekin-agent` instead of using the base image directly. The base image tag is shared across agents.
- The `ramekin-prompt.md` file is written on every run and passed to the agent via `--append-system-prompt`. Both pi and Claude Code accept that flag.
- Image builds fetch `jj`'s latest release tag from the GitHub API. Anonymous calls hit the 60/hour rate limit easily, so ramekin sources a token from the host (`RAMEKIN_GH_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`) and forwards it as a BuildKit secret. The token never lands in `docker history`.
- Version is set at build time via the `RAMEKIN_VERSION` env var (used by CI) or falls back to `dev+<short-sha>`.

## Dependencies

- Production dependencies use `*` (unpinned) versions, except for pre-release crates which pin the exact version (e.g. `serde-kdl2 = "0.1.1-alpha.6"`).
- Dev dependencies also use `*`. Do not pin to the version `cargo add` resolves.
- `Cargo.lock` is committed.

## Version control

- VCS is [jj](https://martinvonz.github.io/jj/latest/), not git.
- Run `just` before committing to catch format, lint, and test failures.
- Each logical change gets its own commit (`jj new -m "..."`). Do not bundle unrelated changes.
- Keep commits out of the working copy: describe with `jj describe -m`, then `jj new` before starting the next change.
- Unrelated files already in the working copy should be restored (`jj restore <path>`) before committing.

## Task management

Tasks are tracked with `ranger` (backlog name: `ramekin`). Use the `ranger` skill for commands and workflow. Pick up the next queued task from the top of the queue unless directed otherwise.

Key commands:
- `ranger task list --backlog ramekin` — list tasks
- `ranger task show <key>` — show task details
- `ranger task edit <key> --state done` — mark complete
