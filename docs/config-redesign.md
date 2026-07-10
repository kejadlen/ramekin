# Config redesign

Status: proposal (2026-07)

## Goals

1. **Multiple agents** — run either pi or Claude Code in the container, selected
   per run or per project, without forking the codebase.
2. **Shareable config** — the interesting parts of my setup (AGENTS.md, skills,
   mounts, agent choice) should live in one place (dotfiles) and be usable from
   any machine and any project.
3. **Overridable everything** — any shared setting can be overridden closer to
   the point of use: per machine, per project, per run.
4. **Safe outbound sharing** — when the agent improves a skill or AGENTS.md
   inside the container, there's a reviewed path for that change to land back
   in the shared source of truth. Never direct writes to host dotfiles.

## What's wrong today

- pi is hardcoded everywhere: the `pi` config section, `/root/.pi` mounts, the
  `pi` entrypoint, `--append-system-prompt` with a file path, the npm package
  in the Dockerfile.
- Only two config files exist (user + project) and there is no way to compose
  them from a shared source. "Sharing" today means hand-symlinking
  `~/.config/ramekin/config.kdl` into dotfiles, with no room for
  machine-specific deviation.
- The agent dir is assembled by *copying* sources in and clearing everything
  (except `auth.json`) on the next run. Config flows one way; in-container
  edits to skills or AGENTS.md silently evaporate.
- No run-level overrides: can't say `--agent claude` or add a one-off mount
  without editing a file.

## Design

### 1. Agents are builtin profiles, config selects one

Since I'm the only user, agent definitions live in Rust, not in config. Each
agent knows its own plumbing:

| | pi | claude |
|---|---|---|
| package | `@mariozechner/pi-coding-agent` | `@anthropic-ai/claude-code` |
| entrypoint | `pi` | `claude` |
| state dir (persisted) | `/root/.pi` | `/root/.claude` + `/root/.claude.json` |
| assembled config dir | `/root/.pi/agent` | `/root/.claude` |
| auth files (preserved on clear) | `auth.json` | `.credentials.json` |
| memory file | `AGENTS.md` | `CLAUDE.md` |
| per-repo sessions | mount at `/root/.pi/agent/sessions` | mount at `/root/.claude/projects` |
| container prompt | `--append-system-prompt <file>` | appended to assembled `CLAUDE.md` |

Notes on the claude column:

- Claude Code's `--append-system-prompt` takes text, not a path, and the
  compose command is baked at generation time. Rather than shell-quoting a
  `$(cat ...)`, ramekin owns assembly anyway — so it appends the
  ramekin-prompt content as a section of the assembled memory file. The same
  mechanism works for pi if we ever want to drop the flag.
- Claude keys session history by project path under `~/.claude/projects/`.
  Every ramekin workspace is `/workspace`, so all projects would collide on
  one key. Mounting the per-repo sessions dir at `/root/.claude/projects`
  keeps histories separate, same trick as pi's sessions mount.
- `~/.claude.json` (onboarding state, MCP servers, project trust) lives
  *next to* the state dir, not in it. The claude profile persists it as a
  file mount from the ramekin data dir.

Selection, lowest to highest precedence: builtin default (`pi`), then
`agent "claude"` in any config layer, then `ramekin run --agent claude`.
The Dockerfile installs both agents; they're small and it keeps one image.

XDG state splits per agent: `$XDG_DATA_HOME/ramekin/pi/`,
`$XDG_DATA_HOME/ramekin/claude/`, and per-repo sessions under
`$XDG_DATA_HOME/ramekin/repos/<slug>/<agent>-sessions/`. Existing pi state
migrates with a one-time move (or just keep `$XDG_DATA_HOME/ramekin/` as the
pi dir and add `claude/` beside it — cheaper, slightly asymmetric).

### 2. Sharing in: `include` + a config search path

Any config file may include others:

```kdl
// ~/.config/ramekin/config.kdl — machine-specific, tiny
include "~/.dotfiles/ramekin/config.kdl"   // the shared base

// machine-only override:
mounts {
    source "~/.local/share/ranger"
    writable
}
```

Rules:

- Included files load as their own layer at *lower* precedence than the file
  that includes them (the includer overrides what it includes).
- Includes may nest; cycles are an error. A missing include is an error too —
  unlike mount sources, a dangling include means the config is wrong, not
  that a host happens to lack a directory.
- `include` accepts a file or a directory (loads `*.kdl` sorted by name).

This makes the shared config a plain directory in dotfiles, versioned with
jj like everything else. New machine setup is one line of user config.

### 3. Overriding: more layers, same merge semantics

Layer order, lowest to highest precedence:

1. **builtin defaults** (agent = pi, builtin mounts — still not overridable)
2. **included files**, in include order
3. **user** `~/.config/ramekin/config.kdl`
4. **project** `<workspace>/.ramekin/config.kdl`
5. **project-local** `<workspace>/.ramekin/config.local.kdl` (gitignored;
   for things true of this checkout on this machine only)
6. **CLI** `--agent`, `--mount src[:target][:rw]`, `--env K=V`

Merge semantics stay what they are: mounts and files dedupe by resolved
target, env by name, scalars (like `agent`) last-writer-wins. The existing
`/dev/null` masking trick remains the way to *remove* an inherited mount.
`ramekin config` already shows which scope won; it grows the new layers.

### 4. Config schema: `pi {}` becomes agent-agnostic `files {}`

```kdl
agent "claude"

mounts {
    source "~/.config/jj"
}

env FOO="bar"

// Copied into the selected agent's config dir at assembly time.
files {
    - { source "~/.dotfiles/ai/_AGENTS.md"; target "@memory" }
    - { source "~/.dotfiles/ai/skills" }
}

// Agent-scoped variants win over unscoped ones for that agent
// and are ignored entirely for other agents.
files agent="claude" {
    - { source "~/.dotfiles/ai/claude-settings.json"; target "settings.json" }
}
```

- `target "@memory"` resolves to the agent's memory file name (`AGENTS.md`
  for pi, `CLAUDE.md` for claude), so one dotfiles source serves both.
  Other targets are plain paths relative to the agent config dir, as today.
- The `pi {}` section keeps parsing for one release with a deprecation warning
  (it maps to `files {}`), then dies. Only my configs need updating.

### 5. Sharing out: the outbox

The agent dir stays copy-in/clear-out — that's what makes runs reproducible.
The new outbound channel is a dedicated writable mount plus a review gate:

- Every session mounts a fresh, empty host dir (under
  `$XDG_DATA_HOME/ramekin/repos/<slug>/outbox/<session-id>/`) at
  `/root/.ramekin/outbox`, writable.
- `ramekin-prompt.md` grows a section telling the agent: to propose a change
  to shared configuration (skills, AGENTS.md, settings), write the changed
  file into the outbox *mirroring the agent-config-dir layout*, and mention
  it to the user. Everything else about the agent dir is throwaway.
- On the host, `ramekin outbox` manages proposals:
  - `ramekin outbox` / `outbox list` — pending proposals across sessions
  - `ramekin outbox diff` — difftastic against the *source* file the `files`
    entry was assembled from (ramekin knows the mapping target → source)
  - `ramekin outbox apply` — copy over the source after confirmation, then
    delete the proposal
  - `ramekin outbox discard`
- After `apply`, the change lands in the dotfiles working copy, where jj
  makes it a normal reviewable/revertable commit.

Why this shape and not the alternatives:

- **Mount dotfiles writable into the container**: simplest, but the whole
  point of ramekin is that the agent can't touch the host outside
  `/workspace`. Rejected.
- **Bare-repo push channel** (container commits to a mounted bare repo, host
  pulls): safest and most jj-native, but a lot of machinery — seeding a repo
  per session, teaching the agent to commit to it, host-side merge UX. The
  outbox gets 90% of the value with a `read_dir` and a copy. The outbox
  layout is also forward-compatible: if it ever feels limiting, the same
  mount point can become a working copy backed by a bare repo without
  changing the agent-facing contract.

Safety properties: the container only ever writes to a session-scoped empty
dir; nothing reaches dotfiles without an explicit `apply`; diffs are against
known sources so a malicious or confused proposal is visible before it lands;
proposals for targets that don't map back to a `files` source are shown but
require an explicit destination to apply.

## Migration sketch

Roughly one commit each, in order:

1. Introduce the builtin agent profile struct; thread it through
   `Ramekin::resolve` replacing hardcoded pi paths/flags. pi remains the only
   profile — pure refactor.
2. Add the claude profile: Dockerfile installs both agents, compose entrypoint
   comes from the profile, memory-file prompt injection, `--agent` flag +
   `agent` config key.
3. Rename `pi {}` → `files {}` with `@memory` target and agent-scoped
   sections; deprecation shim for `pi {}`.
4. Add `include`, project-local layer, and CLI `--mount`/`--env` overrides.
5. Outbox mount + prompt section + `ramekin outbox` subcommand.

Each step lands independently; 1–2 deliver the claude support, 3–4 the
sharing/override story, 5 the outbound channel.

## Open questions

- Does pi's `--append-system-prompt` stay, or do both agents move to
  memory-file injection for symmetry?
- Should `--rebuild`-style flags also become config (e.g. project pins a
  base image)? Not needed yet; the layering makes it cheap to add later.
- One image with both agents vs. per-agent images: starting with one image;
  revisit if the image bloats or the agents' base requirements diverge.
