# Agents

## Project

Ramekin is a containerized harness for running coding agents — the [pi coding agent](https://github.com/badlogic/pi-mono) or Claude Code, selected via profiles. A Rust CLI builds a Docker image carrying both agents, generates a compose config at runtime, and attaches the user's terminal to the agent container. Network restriction via a firewall sidecar is planned but not yet implemented.

## Repository layout

```
Cargo.toml              # Single-crate workspace
src/
  main.rs               # CLI: agent state, image builds, compose generation, outbox commands
  config.rs             # KDL parsing, config layers, profiles, mount resolution and merging
  outbox.rs             # Pending config proposals: scan, map to host sources, apply/discard
build.rs                # Sets RAMEKIN_VERSION from env or git rev
assets/
  Dockerfile            # Base image: both agents (+ claude managed settings, IS_SANDBOX)
  ramekin-prompt.md     # System prompt template appended inside the container
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

- The config redesign is documented in `docs/config-redesign.md`; all four steps of its sequencing are implemented.
- Config files are parsed directly with the `kdl` crate (`parse_config` in `config.rs`), not serde: the grammar is `mounts` blocks, one `env` block syntax (bare child = host passthrough), and `profile` nodes (with children = definition, bare = selection). Unknown nodes fail loudly.
- Config merges layers, lowest precedence first: binary (staples `~/.config/git`/`~/.config/jj` plus the active agent's config allowlist, mounted read-only, canonicalized, skip-if-missing), profile (the active profile's env/mounts), user (every `*.kdl` in `~/.config/ramekin/`, merged as one layer, duplicate keys within the layer are errors), and project (`.ramekin/config.kdl`). A `/dev/null` source masks (removes) a mount inherited from a lower layer. `env` merges per variable; profiles merge by name, last writer takes the whole definition.
- Profiles subsume agent selection: the binary ships trivial `pi`/`claude` profiles, selection precedence is binary < user < project < project-local < `-p`. `Agent` (in `config.rs`) carries each agent's host config dir, allowlist, and container config dir; `AgentState` (in `main.rs`) carries its persistent host paths and session mounts.
- Persistence is per-agent, opposite policies: pi is ephemeral-by-default (fresh session dir at `/root/.pi/agent`; allowlisted `auth.json` from `$XDG_DATA_HOME/ramekin/agents/pi/` and per-repo `sessions/` bound on top; teardown logs discarded writes). Claude is persist-by-default (`~/.claude` + `~/.claude.json` from `$XDG_DATA_HOME/ramekin/agents/`, shared across repos; session-scoped dirs bound over the `CLAUDE_EPHEMERAL` denylist).
- Each workspace mounts at `/workspace/<slug>` (slug is `<dirname>-<hash>`, never a shared `/workspace`) so cwd-keyed agent state stays distinct per repo; compose's `working_dir` puts the agent there on start.
- Docker compose config is generated at runtime via `serde_yaml` over a typed `ComposeConfig` struct, not a static file. Volume mounts use the long-form bind syntax (`{type: bind, source, target, read_only}`), ordered lexicographically by target so parents precede children. Passthrough env vars render as bare names in the environment list.
- One base image (`ramekin-agent`) carries both agents; it has no ENTRYPOINT, and the generated compose config sets `entrypoint` to `pi` or `claude` per session. A project `.ramekin/Dockerfile` builds `FROM ramekin-agent` with a repo-specific tag. Image builds forward a host GitHub token (env vars or `gh auth token`) as a BuildKit secret for API calls.
- The outbox (`src/outbox.rs`) is the only write path for shared config: each session mounts a fresh dir at `/root/.ramekin/outbox`; proposals map back to host sources via the agent allowlist plus an `.agent` sidecar written outside the mount; `ramekin outbox list|diff|apply|discard` reviews them.
- `Ramekin::resolve` is side-effect free (so `ramekin config` never mutates state); materialization happens in `run` via `AgentState::prepare`/`prepare_session`.
- The `ramekin-prompt.md` template is rendered per session (`{{WORKSPACE_PATH}}` → the workspace target), mounted read-only at `/root/.ramekin/ramekin-prompt.md`, and passed via `--append-system-prompt` (pi) / `--append-system-prompt-file` (Claude — its plain flag takes a literal string).
- Version is set at build time via the `RAMEKIN_VERSION` env var (used by CI) or falls back to `dev+<short-sha>`.

## Dependencies

- Production dependencies use `*` (unpinned) versions, except for pre-release crates which pin the exact version.
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
