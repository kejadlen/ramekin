# Config redesign

Status: proposal (2026-07). The `claude-code` branch is a prototype to learn
from here, not a baseline to preserve.

## Goals

1. **Multiple agents** — run either pi or Claude Code in the container,
   selected per project or per run.
2. **Shareable config** — the interesting parts of my setup (memory files,
   skills, mounts, agent choice) live in one place (dotfiles) and are usable
   from any machine and any project.
3. **Overridable everything** — any shared setting can be overridden closer
   to the point of use: per machine, per project, per run.
4. **Immutable config, outbox for everything stateful** — configuration is
   read-only inside the container; in-container edits to it are deliberately
   impossible, not merely ephemeral. The outbox is the single reviewed path
   for any stateful modification to shared config. Dotfiles are never
   writable from inside the container.
5. **Multiple sessions** — concurrent ramekin runs (same repo or different,
   same agent or different) don't interfere with each other.

## Lessons from the prototypes

From `main`:

- Layered KDL config with per-target merge and scope-labelled `ramekin
  config` output works well; keep the shape.
- Copy-and-clear assembly of the agent dir doesn't survive this redesign,
  for two reasons. The fatal one is concurrency: a second session's clear
  yanks files out from under the first (goal 5). The subtle one is that it
  gets goal 4's semantics *almost* right — in-container edits to config
  should not stick, and with copies they don't — but silently: the agent can
  edit its config, believe it worked, and lose it on the next run. Read-only
  mounts enforce the same intent loudly, at write time, which is what steers
  the agent to the outbox.

From `claude-code`:

- **Read-only bind mounts beat copies** for exposing shared config: host
  edits stay live, nothing to reassemble, nothing to clear. (`claude { ... }`
  proved this; the redesign extends it to pi.)
- **Per-slug workspace mounts** (`/workspace/<slug>`) isolate cwd-keyed agent
  state (Claude's `projects` map, transcripts) while auth and identity stay
  global. Cheaper and more robust than splitting agent state files.
- Yolo mode belongs in the image (managed settings + `IS_SANDBOX=1`), not in
  CLI flags.
- Keep: side-effect-free `ramekin config`, deterministic parent-before-child
  mount ordering, long-form compose bind syntax, GitHub token as BuildKit
  secret.
- Anti-lesson: both agents building to one `ramekin-agent` tag means
  switching agents silently redefines the tag and busts project layers built
  on the other agent. Images must be per-agent.

## Design

### Agents

`agent "pi" | "claude"` is a top-level KDL scalar; the highest-precedence
layer that sets it wins, and `ramekin run --agent <a>` beats them all.
Default: pi.

Agent definitions live in Rust (builtin profiles), covering: base Dockerfile,
image tag, entrypoint, state dir layout, config-dir path in the container,
prompt injection flag, auth file names. Config never defines an agent, only
selects one.

Base images are tagged per agent: `ramekin-pi`, `ramekin-claude`. A project
`.ramekin/Dockerfile` declares `ARG BASE` / `FROM ${BASE}` and ramekin passes
the active agent's tag as the build arg, so one project Dockerfile serves
both agents. Project image tags stay repo-specific (per main) and gain an
agent suffix.

### Config layers

Lowest to highest precedence:

1. **builtin defaults** (agent = pi; builtin mounts stay non-overridable)
2. **included files**, in include order
3. **user** `~/.config/ramekin/config.kdl`
4. **project** `<workspace>/.ramekin/config.kdl`
5. **project-local** `<workspace>/.ramekin/config.local.kdl` (gitignored)
6. **CLI** — `--agent`; `--mount`/`--env` when they earn their keep

Merge semantics as today: mounts and config entries dedupe by resolved
target, env by name, scalars last-writer-wins, `/dev/null` masking removes
an inherited mount.

`include` is the sharing mechanism:

```kdl
// ~/.config/ramekin/config.kdl — machine-specific, tiny
include "~/.dotfiles/ramekin/config.kdl"   // the shared base

// machine-only overrides below
mounts {
    source "~/.local/share/ranger"
    writable
}
```

Included files load at *lower* precedence than the includer; includes nest;
cycles and missing files are errors (unlike mount sources, a dangling include
means the config is wrong, not that a host lacks a directory). `include`
accepts a file or a directory (`*.kdl`, sorted).

### Agent config entries: read-only bind mounts for both agents

One vocabulary, symmetric across agents:

```kdl
pi {
    source "~/.dotfiles/ai/AGENTS.md"
}
claude {
    source "~/.dotfiles/ai/CLAUDE.md"
}
claude {
    source "~/.dotfiles/ai/skills"
    target "skills"
}
```

An entry expands to a **read-only** bind mount at `<config-dir>/<target>`
(target defaults to the source basename). Only the block matching the active
agent applies; the other is inert. No `writable` field — shared config is
read-only by design (goal 4); the outbox is the write path.

### Session model

The design distinguishes two kinds of mutation. **Agent runtime state**
(auth tokens, account identity, session history) persists via direct
writable mounts — the agent owns it and it must survive every run.
**Configuration** (memory files, skills, settings sourced from dotfiles) is
immutable in the container; the outbox is its only write path. Everything a
run touches falls into one of three buckets:

- **Persistent, shared across sessions:** the agent state dirs —
  `$XDG_DATA_HOME/ramekin/agents/pi/` and `agents/claude/` (+
  `agents/claude.json`), mounted writable at `/root/.pi` / `/root/.claude`
  (+ `/root/.claude.json`). Concurrent access here is the agent's own
  problem, and both agents already handle multiple simultaneous sessions on
  a normal host. Auth, identity, and history live here and survive
  everything.
- **Shared config, read-only:** the bind-mounted entries above. Immutable
  from the container, so concurrent sessions can't fight over them.
- **Session-scoped:** a per-session dir (as today: random id) holding the
  compose file, the rendered `ramekin-prompt.md` (mounted read-only at the
  agent's prompt path), and the **outbox**. Pi's config dir stops being a
  shared host dir that gets cleared: each session mounts a fresh, empty
  writable session dir at `/root/.pi/agent`, with the config entries,
  `auth.json` (file mount from persistent state), and `sessions/` (per-repo
  persistent dir) bind-mounted on top. Nothing is ever cleared; the dir is
  simply new each time and discarded on teardown.

Workspaces mount at `/workspace/<slug>` for both agents (uniformity; Claude
requires it, pi doesn't care). Compose project names keep the session id.
Concurrent image builds of the same tag are idempotent in Docker; per-agent
tags remove the pi/claude race.

### Outbox

The only outbound channel, same for both agents:

- Every session mounts its fresh, empty outbox dir (host:
  `$XDG_DATA_HOME/ramekin/repos/<slug>/outbox/<session-id>/`) at
  `/root/.ramekin/outbox`, writable — the only agent-writable path outside
  the workspace and the agent state mounts.
- `ramekin-prompt.md` tells the agent: shared config is read-only by design;
  to propose a change, write the changed file into the outbox mirroring the
  config-dir layout and tell the user.
- Host side, `ramekin outbox`:
  - `list` — pending proposals across sessions
  - `diff` — difftastic against the source each entry was mounted from
    (ramekin knows the target → source mapping)
  - `apply` — copy over the source after confirmation, drop the proposal
  - `discard`
- After `apply`, the change is an ordinary working-copy edit in dotfiles;
  jj takes it from there.

Safety: the container writes only to a session-scoped empty dir; nothing
reaches dotfiles without explicit `apply`; diffs run against known sources so
a confused or malicious proposal is visible before it lands; proposals that
don't map back to a configured source need an explicit destination to apply.

## Sequencing

Build on main; harvest `claude-code` commits where they fit rather than
rebasing the branch wholesale (its copy-vs-mount asymmetry and shared tag
don't survive the redesign, but the claude Dockerfile, managed settings,
per-slug workspace, compose long-form binds, BuildKit secret, and
side-effect-free `config` all do):

1. Session model refactor on pi only: per-session agent dir, entries become
   read-only bind mounts, `pi {}` loses copy semantics. Multi-session works
   from here on.
2. Claude support: profile, `Dockerfile.claude` (harvested), per-agent tags +
   `ARG BASE` project builds, `--agent` flag + `agent` scalar.
3. `include` + project-local layer.
4. Outbox: mount + prompt section, then the `ramekin outbox` subcommand.

## Open questions

- Does pi tolerate a read-only `AGENTS.md`/`skills/` in its agent dir, and
  does it write scratch files there at runtime? The fresh writable session
  dir underneath the read-only binds should absorb anything, but verify
  before committing to step 1.
- `--mount`/`--env` CLI flags: deferred until a real need shows up.
- Firewall sidecar (long-planned) intersects with the session model — each
  session's compose stack is where it would attach. Out of scope here.
