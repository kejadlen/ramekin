# Agents

## Project

Ramekin is a containerized harness for running the [pi coding agent](https://github.com/badlogic/pi-mono). A Rust CLI builds a Docker image, generates a compose config at runtime, and attaches the user's terminal to the agent container. Network restriction via a firewall sidecar is planned but not yet implemented.

## Repository layout

```
Cargo.toml              # Single-crate workspace
src/
  main.rs               # CLI: builds image, generates compose, starts container, attaches
  config.rs             # KDL config layers, mount resolution and merging
build.rs                # Sets RAMEKIN_VERSION from env or git rev
assets/
  Dockerfile            # Agent container image (Node.js + pi + jj + Rust)
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

- The config redesign is documented in `docs/config-redesign.md`; the session model, layer structure, and persistence policy below implement its step 1 (pi only).
- Docker compose config is generated at runtime via `serde_yaml` over a typed `ComposeConfig` struct, not a static file. Volume mounts use the long-form bind syntax (`{type: bind, source, target, read_only}`), which sidesteps the colon-delimited `source:target[:ro]` format and its quoting hazards. Mounts are ordered lexicographically by target so parents precede children.
- Config merges three layers, lowest precedence first: binary (compiled-in staples `~/.config/git`/`~/.config/jj` plus the pi agent-config allowlist `~/.pi/agent/{AGENTS.md,skills}`, mounted read-only, canonicalized, skip-if-missing), user (`~/.config/ramekin/config.kdl`), and project (`<workspace>/.ramekin/config.kdl`). A `/dev/null` source masks (removes) a mount inherited from a lower layer.
- Pi state is ephemeral by default: each session mounts a fresh empty dir at `/root/.pi/agent`, with persistent pieces bound on top — `auth.json` from `$XDG_DATA_HOME/ramekin/agents/pi/`, per-repo `sessions/` from `$XDG_DATA_HOME/ramekin/repos/<slug>/sessions/` (slug is `<dirname>-<hash>`). On teardown, anything else the agent wrote to the session dir is logged before being discarded.
- Each workspace mounts at `/workspace/<slug>` (never a shared `/workspace`) so cwd-keyed agent state stays distinct per repo; compose's `working_dir` puts the agent there on start.
- `Ramekin::resolve` is side-effect free (so `ramekin config` never mutates state); `Ramekin::prepare`, called from `run`, creates directories and initializes/migrates `auth.json`.
- If the workspace contains `.ramekin/Dockerfile`, the CLI builds it on top of `ramekin-agent` instead of using the base image directly.
- The `ramekin-prompt.md` template is rendered per session (`{{WORKSPACE_PATH}}` → the workspace target), mounted read-only into the agent dir, and passed to pi via `--append-system-prompt`.
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
