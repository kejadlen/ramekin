# Config redesign

Status: proposal (2026-07). Ramekin is single-user; this design leans on that
hard. The `claude-code` branch is a prototype to learn from, not a baseline
to preserve.

## Goals

1. **Agents × providers** — run pi or Claude Code, against the right backend
   per context: claude with anthropic or bedrock; pi with anthropic or GLM.
2. **Multiple machines** — the same setup works on every machine; machines
   differ only where they genuinely differ (default provider, credentials).
3. **Overridable everything** — any default can be overridden closer to the
   point of use: per machine, per project, per run.
4. **Immutable config, outbox for everything stateful** — configuration is
   read-only inside the container; in-container edits to it are deliberately
   impossible, not merely ephemeral. The outbox is the single reviewed path
   for stateful modification of shared config. Agent runtime state (auth,
   history) persists via direct writable mounts. Dotfiles are never writable
   from inside the container.
5. **Multiple sessions** — concurrent ramekin runs (same repo or different,
   same profile or different) don't interfere with each other.

## Principles

Three tiers, by rate of change:

- **The binary** holds globals — anything stable across machines and
  projects that's a fact about the tools or their one user: agent plumbing,
  staple mounts, tree layout, session model. Changing these is an edit +
  `just install`, which is the loop this repo already lives in. Config files
  never restate them.
- **The filesystem** holds content — memory files, skills, settings, env
  files — as real files in conventional locations. Sharing across machines
  is a dotfiles symlink, not a ramekin feature.
- **KDL** holds the remainder — profile definitions and the genuinely
  irregular, mostly per-project: extra mounts, masks, a profile override.
  Small, optional, and mostly living in the shared user tree.

## Lessons from the prototypes

From `main`: layered config with per-target merge and scope-labelled
`ramekin config` output works; keep the shape. Copy-and-clear assembly dies
here — a second session's clear yanks files from under the first (goal 5),
and its silent-loss semantics for in-container edits had the right intent
(goal 4) but the wrong failure mode: read-only mounts enforce the same
policy loudly, at write time, steering the agent to the outbox.

From `claude-code`: read-only bind mounts beat copies (host edits live,
nothing to clear); per-slug workspace mounts isolate Claude's cwd-keyed
state while auth stays global; yolo mode belongs in the image (managed
settings + `IS_SANDBOX=1`); keep side-effect-free `ramekin config`,
deterministic parent-before-child mount ordering, long-form compose binds,
and the GitHub-token BuildKit secret. Anti-lesson: one shared image tag
across agents silently redefines `FROM ramekin-agent` — images must be
per-agent.

## Design

### Profiles: KDL bundles of agent + provider

A profile is a named bundle: agent, env vars, extra mounts. The binary
ships only the two trivial ones — `pi` and `claude`, bare agent with no
provider plumbing — so ramekin runs with zero config. Everything richer is
defined in KDL, normally in the user tree so it shares via dotfiles:

```kdl
// ~/.config/ramekin/config.kdl
profile "claude-bedrock" {
    agent "claude"
    env CLAUDE_CODE_USE_BEDROCK="1"
    env AWS_PROFILE            // bare = pass through the host value
    mounts { source "~/.aws" }
}

profile "pi-glm" {
    agent "pi"
    env ANTHROPIC_BASE_URL="https://open.bigmodel.cn/api/anthropic"
    env ZHIPU_API_KEY
}
```

Profiles merge by name across layers, last writer takes the whole
definition — a project can redefine `claude-bedrock` wholesale, but
fine-grained tweaks (one env var) go through the ordinary layered `env`
instead, which overlays whatever profile is active. There is no claude-GLM
profile because that combination isn't wanted; adding one later is four
lines of KDL, not a release.

Selection, lowest to highest precedence:

1. binary default (`pi`)
2. `RAMEKIN_PROFILE` env var — the per-machine default, set in each
   machine's shell config, which dotfiles already manage. The work machine
   exports `claude-bedrock`; home exports `claude` or `pi-glm`. Ramekin
   needs no per-machine config file.
3. `profile "pi"` in project KDL — this repo wants this profile
4. `ramekin -p pi-glm` — this run wants this profile

Profile selection subsumes agent selection; there is no separate `--agent`.
Model choice *within* a provider stays out of profiles — that's per-run
agent args after `--`.

Provider credentials never appear in config: passthrough env forwards host
values at run time, the AWS mount carries its own files, and OAuth lives in
persistent agent state. Layered `env` (below) can adjust a profile's
variables (e.g. a different `AWS_PROFILE` for one project) without defining
new profiles.

### Staple mounts: hardcoded, overridable

The mounts every machine wants — `~/.config/git`, `~/.config/jj` read-only,
`~/.local/share/ranger` writable — move into the binary as a builtin layer.
Mount resolution already skips missing sources, so machines lacking one pay
nothing. Unlike the workspace/state/outbox mounts (still non-overridable),
staples sit at the *bottom* of the precedence order: any tree can override
one by target or mask it with `/dev/null`.

### Config trees: content by convention

Two trees, at hardcoded locations. A tree is a directory:

```
<tree>/
  config.kdl    # profile definitions/selection, mounts — nothing else
  env           # KEY=value, or bare KEY to pass through the host value
  pi/           # contents land in /root/.pi/agent/, read-only
  claude/       # contents land in /root/.claude/, read-only
  Dockerfile    # project image layer (project tree only)
```

- **user** `~/.config/ramekin/` — shared across machines by symlinking it
  (or entries within it) into dotfiles. No `include` keyword: the symlink is
  the include, managed by the same mechanism as every other dotfile.
- **project** `<workspace>/.ramekin/` — committed to the repo.
- **project-local** `<workspace>/.ramekin/local/` — gitignored nested tree
  for facts about this checkout on this machine; can override content as
  well as KDL.

Adding a skill is dropping a directory into `claude/skills/` — no KDL.
Symlinks inside a tree work (`claude/CLAUDE.md → ../ai/CLAUDE.md`), so agent
subtrees share sources without duplication; ramekin canonicalizes per entry
when generating mounts.

Precedence, lowest to highest: binary (staples, trivial profiles) → user →
project → project-local → CLI. Merging: agent-tree contents dedupe by
top-level entry name (`CLAUDE.md`, `skills/`, `settings.json`), higher tree
wins the whole entry; `env` merges per variable, overlaying the active
profile's env, so any tree can adjust one variable without redefining the
profile; KDL mounts dedupe by resolved target; scalars last-writer-wins.

At run time each winning agent-tree entry becomes a **read-only** bind mount
in the active agent's config dir. Only the subtree matching the active
profile's agent applies; the other is inert. There is no writable variant —
the outbox is the write path.

### Session model

Every path a run touches is one of three kinds:

- **Persistent agent state** (writable, shared across sessions):
  `$XDG_DATA_HOME/ramekin/agents/pi/` and `agents/claude/` (+
  `agents/claude.json`) mounted at `/root/.pi` / `/root/.claude` (+
  `/root/.claude.json`). Auth, identity, history. Concurrent access is the
  agent's own problem, which both agents already handle on a normal host.
  State is per-agent, not per-profile — pi's auth file holds multiple
  providers by design, and claude's OAuth coexists with bedrock env.
- **Config** (read-only): staple mounts and agent-tree entries. Immutable
  from the container, so sessions can't fight over it.
- **Session-scoped** (fresh per run, discarded on teardown): the compose
  file and project name (random session id, as today), the rendered
  `ramekin-prompt.md` (mounted read-only at the agent's prompt path), the
  **outbox**, and pi's agent dir — a fresh empty writable dir per session
  mounted at `/root/.pi/agent`, with config entries, `auth.json`, and the
  per-repo `sessions/` dir bind-mounted on top. Nothing is ever cleared;
  the dir is new each time.

Workspaces mount at `/workspace/<slug>` for both agents (Claude needs the
cwd isolation; pi doesn't care; uniformity wins). Base images build to
per-agent tags (`ramekin-pi`, `ramekin-claude`); a project
`.ramekin/Dockerfile` declares `ARG BASE` / `FROM ${BASE}` and ramekin
passes the active agent's tag, so one project Dockerfile serves both.
Project image tags stay repo-specific and gain the agent suffix. Concurrent
builds of the same tag are idempotent; per-agent tags remove the pi/claude
race.

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
  - `diff` — difftastic against the source each entry was mounted from (a
    proposal's relative path maps straight back to the winning config tree)
  - `apply` — copy over the source after confirmation, drop the proposal
  - `discard`
- After `apply`, the change is an ordinary working-copy edit in dotfiles;
  jj takes it from there.

Safety: the container writes only to a session-scoped empty dir; nothing
reaches dotfiles without explicit `apply`; diffs run against known sources
so a confused or malicious proposal is visible before it lands; proposals
that don't map back to a configured source need an explicit destination to
apply.

## Sequencing

Build on main; harvest `claude-code` commits where they fit (claude
Dockerfile + managed settings, per-slug workspace, long-form binds, BuildKit
secret, side-effect-free `config`) rather than rebasing the branch:

1. Session model refactor on pi only: per-session agent dir, config trees
   replace the `pi {}` block, tree entries become read-only bind mounts,
   staples move into the binary. Multi-session works from here on.
2. Claude support: agent plumbing, `Dockerfile.claude` (harvested),
   per-agent tags + `ARG BASE` project builds.
3. Profiles: KDL `profile` blocks + builtin trivial profiles,
   `RAMEKIN_PROFILE`, project `profile` scalar, `-p`, env passthrough.
4. Outbox: mount + prompt section, then the `ramekin outbox` subcommand.

## Open questions

- Does pi tolerate a read-only `AGENTS.md`/`skills/` in its agent dir, and
  where does it write scratch files at runtime? The fresh writable session
  dir underneath the read-only binds should absorb anything — verify before
  step 1.
- Merge granularity for agent trees: top-level entry (proposed) means a
  project overriding one skill shadows the whole `skills/` dir. Per-file
  merging fixes that at the cost of many more mounts. Start coarse.
- Firewall sidecar (long-planned) intersects with the session model — each
  session's compose stack is where it would attach. Out of scope here.
