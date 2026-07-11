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
- **The filesystem** holds content — memory files, skills, settings — as
  real files in conventional locations. Sharing across machines is a
  dotfiles symlink, not a ramekin feature.
- **KDL** holds the remainder — profile definitions, env, and the genuinely
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
2. `profile "claude-bedrock"` in user KDL — the per-machine default. The
   work machine's user config names `claude-bedrock`; home names `claude`
   or `pi-glm`.
3. `profile "pi"` in project KDL — this repo wants this profile
4. `ramekin -p pi-glm` — this run wants this profile

The machine default living in the user tree means that tree is *not*
symlinked wholesale into dotfiles: profile definitions and content entries
are shared (symlinked per entry, or via whatever host-specific mechanism
the dotfiles already use), while the `profile` selection line stays
machine-local. Symlink granularity is a dotfiles decision, not a ramekin
one.

Profile selection subsumes agent selection; there is no separate `--agent`.
Model choice *within* a provider stays out of profiles — that's per-run
agent args after `--`.

Provider credentials never appear in config: passthrough env forwards host
values at run time, the AWS mount carries its own files, and OAuth lives in
persistent agent state. Layered `env` (below) can adjust a profile's
variables (e.g. a different `AWS_PROFILE` for one project) without defining
new profiles.

### Staple mounts: hardcoded, overridable

The mounts every machine wants — `~/.config/git` and `~/.config/jj`,
read-only — move into the binary as a builtin layer. Mount resolution
already skips missing sources, so machines lacking one pay nothing. Unlike
the workspace/state/outbox mounts (still non-overridable), staples sit at
the *bottom* of the precedence order: any tree can override one by target or
mask it with `/dev/null`.

The bar for a staple is "true on every machine". Ranger, for example, isn't
— it stays a user-KDL mount on the machines that have it:

```kdl
mounts {
    source "~/.local/share/ranger"
    writable
}
```

### Config trees: content by convention

Two trees, at hardcoded locations. A tree is a directory:

```
<tree>/
  config.kdl    # profile definitions/selection, mounts, env
  pi/           # contents land in /root/.pi/agent/, read-only
  claude/       # contents land in /root/.claude/, read-only
  Dockerfile    # project image layer (project tree only)
```

Env stays in KDL rather than a dotenv file: profile env is already KDL, so
a separate file would split one concern across two syntaxes, and the
bare-name passthrough form (`env AWS_PROFILE`) isn't standard dotenv anyway.
The line between KDL and filesystem is *file-shaped content* (memory files,
skills, settings) versus *key-value facts* — env is the latter.

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

Every path a run touches is either read-only **config** (staple mounts and
agent-tree entries — immutable from the container, so sessions can't fight
over it), **session plumbing** (the compose file and project name under a
random session id, the rendered `ramekin-prompt.md` mounted read-only, the
outbox), or **agent runtime state** — which is not one bucket but three:

| scope | claude | pi |
|---|---|---|
| global | `.credentials.json` (OAuth); `~/.claude.json` (identity, onboarding, MCP servers) | `auth.json` |
| per-project | `projects/<cwd>/` transcripts; the `projects` map entries inside `~/.claude.json` | `sessions/` (already split per-repo) |
| ephemeral | `statsig/`, `todos/`, `shell-snapshots/`, `debug/` — caches and scratch | anything else it writes in its agent dir |

State is per-agent, not per-profile: pi's auth file holds multiple providers
by design, and claude's OAuth coexists with bedrock env. Concurrent access
to the global bucket is the agent's own problem, which both agents already
handle on a normal host.

The two agents get opposite persistence policies, chosen by failure mode:

- **claude: persist by default, denylist the junk.** `~/.claude` and
  `~/.claude.json` mount persistently from `$XDG_DATA_HOME/ramekin/agents/`
  (global bucket), with fresh session-scoped dirs bound *over* the known
  ephemeral subdirs. `~/.claude.json` can't be split by mounts — it mixes
  global identity with per-project trust — but doesn't need to be: the
  per-slug workspace mount partitions its `projects` map by cwd, and the
  transcript dirs under `projects/` partition the same way. If claude grows
  a new state file we haven't classified, it persists (worst case: rot)
  rather than vanishing (worst case: lost auth, mystery re-onboarding).
- **pi: ephemeral by default, allowlist what persists.** A fresh empty
  writable dir per session at `/root/.pi/agent`, with `auth.json` and the
  per-repo `sessions/` dir bind-mounted on top and read-only config entries
  above that. Nothing is ever cleared; the dir is new each time. Pi's
  persistent surface is small and stable enough that the allowlist risk is
  acceptable, and the session dir is already forced by config immutability.

One mechanical caveat shapes the bucket boundaries: agents update state
files by write-temp-then-rename, and a rename onto a bind-mounted *file*
replaces the inode out from under the mount. Buckets should sit at
directory granularity wherever the agent does atomic writes; the
`~/.claude.json` single-file mount needs verifying under claude's write
pattern (escape hatch: `CLAUDE_CONFIG_DIR` relocates it into a directory we
control). Same question for pi's `auth.json` before trusting its file bind.

**Teardown visibility** keeps either policy honest: on session end, ramekin
diffs the session-scoped dirs and logs anything novel the agent wrote that
is about to be discarded. That's the learning loop for promoting a path
into the persistent set — or confirming it's junk — instead of discovering
the omission via broken onboarding weeks later. Durable state never appears
silently; it rhymes with the outbox.

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
   staples move into the binary, teardown report on discarded session-dir
   writes. Multi-session works from here on.
2. Claude support: agent plumbing, `Dockerfile.claude` (harvested),
   per-agent tags + `ARG BASE` project builds, ephemeral denylist mounts
   over `~/.claude` junk.
3. Profiles: KDL `profile` blocks + builtin trivial profiles, user-KDL
   machine default, project `profile` scalar, `-p`, env passthrough.
4. Outbox: mount + prompt section, then the `ramekin outbox` subcommand.

## Open questions

- Does pi tolerate a read-only `AGENTS.md`/`skills/` in its agent dir, and
  where does it write scratch files at runtime? The fresh writable session
  dir underneath the read-only binds should absorb anything — verify before
  step 1.
- Single-file bind mounts vs atomic renames: verify claude's write pattern
  for `~/.claude.json` and pi's for `auth.json` before trusting file binds;
  fall back to `CLAUDE_CONFIG_DIR` / directory-level mounts if renames break
  them.
- The claude ephemeral denylist (`statsig/`, `todos/`, `shell-snapshots/`,
  `debug/`) is a best-current-guess; the teardown report exists to refine it.
- Merge granularity for agent trees: top-level entry (proposed) means a
  project overriding one skill shadows the whole `skills/` dir. Per-file
  merging fixes that at the cost of many more mounts. Start coarse.
- Firewall sidecar (long-planned) intersects with the session model — each
  session's compose stack is where it would attach. Out of scope here.
