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
  which agent files are config vs state, staple mounts, session model.
  Changing these is an edit + `just install`, which is the loop this repo
  already lives in. Config files never restate them.
- **The host's existing files** hold content. Agent config (memory files,
  skills, settings) lives where the agents already look for it locally —
  `~/.claude/`, `~/.pi/agent/` — managed by dotfiles like everything else.
  Ramekin maintains no parallel copy of any of it.
- **KDL** holds the remainder — profile definitions, env, and the genuinely
  irregular, mostly per-project: extra mounts, masks, a profile override.
  Small, optional.

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

### Agent config: the host's own, mounted read-only

The agents are also used locally, so their config already exists on the host
in the dirs they define — no ramekin-owned tree duplicates it. The binary
carries an allowlist per agent of the *config-shaped* entries in those dirs:

- claude: `~/.claude/CLAUDE.md`, `settings.json`, `skills/`, `agents/`,
  `commands/` (exact list to finalize)
- pi: `~/.pi/agent/AGENTS.md`, `skills/` (exact list to finalize)

Each entry that exists on the host mounts **read-only** at its normal path
inside the container, layered over the container's own persistent agent
state. Missing entries are skipped, like staples. The allowlist matters
because the same host dirs also hold runtime state — host credentials,
transcripts, caches — which must *not* leak into the container; the
container has its own persistent state (below).

Project-level agent config costs ramekin nothing: it rides the workspace
mount, and both agents natively layer their own project config (`.claude/`,
`CLAUDE.md` / `AGENTS.md` in the repo). Ramekin adds no merge machinery on
top of layering the agents already implement.

When some host config shouldn't apply in the container (say, host
`settings.json` hooks that invoke host-only tools), ordinary KDL mounts
override or mask it: mount a different source at that target, or `/dev/null`
it. In-container edits to any of this fail loudly — the outbox is the write
path.

Host agent dirs commonly symlink into dotfiles; ramekin canonicalizes each
entry when generating mounts, since bind sources need real paths.

### Profiles: KDL bundles of agent + provider

A profile is a named bundle: agent, env vars, extra mounts. The binary
ships only the two trivial ones — `pi` and `claude`, bare agent with no
provider plumbing — so ramekin runs with zero config. Everything richer is
defined in KDL:

```kdl
// ~/.config/ramekin/profiles.kdl — symlinked from dotfiles, shared
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
2. `profile "claude-bedrock"` in user KDL — the per-machine default
3. `profile "pi"` in project KDL — this repo wants this profile
4. `ramekin -p pi-glm` — this run wants this profile

Profile selection subsumes agent selection; there is no separate `--agent`.
Model choice *within* a provider stays out of profiles — that's per-run
agent args after `--`.

Provider credentials never appear in config: passthrough env forwards host
values at run time, the AWS mount carries its own files, and OAuth lives in
the container's persistent agent state.

### Config files: three KDL layers

KDL carries profiles (definition and selection), `env`, and `mounts` —
nothing content-shaped. Layers, lowest to highest precedence:

1. **binary** — staples (`~/.config/git`, `~/.config/jj`, read-only,
   skip-if-missing, overridable by any layer; the bar for a staple is
   "true on every machine" — ranger isn't, so it stays user KDL), trivial
   profiles, the agent-config allowlist
2. **user** — every `*.kdl` in `~/.config/ramekin/`, merged as one layer;
   defining the same key twice within the layer is an error. Sharing is
   per-file symlinks into dotfiles: `profiles.kdl` is shared, while
   `config.kdl` stays machine-local and holds the machine's default
   `profile` selection and machine-only mounts (ranger). Symlink
   granularity is a dotfiles decision, not a ramekin one.
3. **project** — `<workspace>/.ramekin/config.kdl`, committed
4. **project-local** — `<workspace>/.ramekin/config.local.kdl`, gitignored
5. **CLI** — `-p`; `--mount`/`--env` when they earn their keep

Merging: `env` merges per variable, overlaying the active profile's env, so
any layer can adjust one variable without redefining the profile; mounts
dedupe by resolved target; profiles by name; scalars last-writer-wins;
`/dev/null` masking removes an inherited mount. A project `Dockerfile`
stays at `.ramekin/Dockerfile`, beside the KDL.

### Session model

Every path a run touches is either read-only **config** (staple mounts and
host agent-config mounts — immutable from the container, so sessions can't
fight over it), **session plumbing** (the compose file and project name
under a random session id, the rendered `ramekin-prompt.md` mounted
read-only, the outbox), or **agent runtime state** — which is not one
bucket but three:

| scope | claude | pi |
|---|---|---|
| global | `.credentials.json` (OAuth); `~/.claude.json` (identity, onboarding, MCP servers) | `auth.json` |
| per-project | `projects/<cwd>/` transcripts; the `projects` map entries inside `~/.claude.json` | `sessions/` (already split per-repo) |
| ephemeral | `statsig/`, `todos/`, `shell-snapshots/`, `debug/` — caches and scratch | anything else it writes in its agent dir |

Container runtime state is the container's own, persisted under
`$XDG_DATA_HOME/ramekin/agents/` — deliberately separate from the host
agent state sitting next to the mounted host config. State is per-agent,
not per-profile: pi's auth file holds multiple providers by design, and
claude's OAuth coexists with bedrock env. Concurrent access to the global
bucket is the agent's own problem, which both agents already handle on a
normal host.

The two agents get opposite persistence policies, chosen by failure mode:

- **claude: persist by default, denylist the junk.** `~/.claude` and
  `~/.claude.json` mount persistently from `$XDG_DATA_HOME/ramekin/agents/`
  (global bucket), with fresh session-scoped dirs bound *over* the known
  ephemeral subdirs, and the host-config mounts (read-only) above that.
  `~/.claude.json` can't be split by mounts — it mixes global identity with
  per-project trust — but doesn't need to be: the per-slug workspace mount
  partitions its `projects` map by cwd, and the transcript dirs under
  `projects/` partition the same way. If claude grows a new state file we
  haven't classified, it persists (worst case: rot) rather than vanishing
  (worst case: lost auth, mystery re-onboarding).
- **pi: ephemeral by default, allowlist what persists.** A fresh empty
  writable dir per session at `/root/.pi/agent`, with `auth.json` and the
  per-repo `sessions/` dir bind-mounted on top and the read-only host-config
  mounts above that. Nothing is ever cleared; the dir is new each time.
  Pi's persistent surface is small and stable enough that the allowlist
  risk is acceptable, and the session dir is already forced by config
  immutability.

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

Workspaces mount at a per-repo path derived from the slug — `/workspace/<slug>`
— for both agents, never a shared `/workspace`. Anything either agent keys
by cwd (claude's `projects` map and transcripts, pi's session grouping)
needs distinct paths per repo; a fixed `/workspace` makes every repo look
like the same project. Whether the slug sits under a `/workspace/` parent
or at the root is cosmetic — the slug does the isolating — but the stable
parent keeps the "changes under here reach the host" story simple in the
rendered prompt. A possible simplification falls out: if pi groups sessions
by cwd on its own, distinct workspace paths may make ramekin's per-repo
`sessions/` mount redundant — verify against pi's actual layout.

Base images build to
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
  agent config layout and tell the user.
- Host side, `ramekin outbox`:
  - `list` — pending proposals across sessions
  - `diff` — difftastic against the host source each entry was mounted from
    (a proposal's relative path maps straight back to the host agent dir)
  - `apply` — copy over the host source after confirmation, drop the
    proposal; when the source is a dotfiles symlink, apply writes through
    to the target, where jj picks it up as a normal working-copy edit
  - `discard`
- Safety: the container writes only to a session-scoped empty dir; nothing
  reaches host config without explicit `apply`; diffs run against known
  sources so a confused or malicious proposal is visible before it lands;
  proposals that don't map back to an allowlisted entry need an explicit
  destination to apply.

## Sequencing

Build on main; harvest `claude-code` commits where they fit (claude
Dockerfile + managed settings, per-slug workspace, long-form binds, BuildKit
secret, side-effect-free `config`) rather than rebasing the branch:

1. Session model refactor on pi only: per-session agent dir, host
   agent-config mounts replace the `pi {}` block, staples move into the
   binary, teardown report on discarded session-dir writes. Multi-session
   works from here on.
2. Claude support: agent plumbing, `Dockerfile.claude` (harvested),
   per-agent tags + `ARG BASE` project builds, ephemeral denylist mounts
   over `~/.claude` junk.
3. Profiles: KDL `profile` blocks + builtin trivial profiles, user-KDL
   machine default, project `profile` scalar, `-p`, env passthrough,
   user layer reads `*.kdl`.
4. Outbox: mount + prompt section, then the `ramekin outbox` subcommand.

## Open questions

- Finalize the agent-config allowlists: which entries of `~/.claude/` and
  `~/.pi/agent/` are config-shaped (claude `hooks/`? `output-styles/`? pi
  extensions, models config?). Skip-if-missing makes over-inclusion cheap.
- Does pi tolerate a read-only `AGENTS.md`/`skills/` in its agent dir, and
  where does it write scratch files at runtime? The fresh writable session
  dir underneath the read-only binds should absorb anything — verify before
  step 1.
- Does pi key sessions by cwd? If so, per-slug workspace mounts may obsolete
  ramekin's per-repo `sessions/` mount (see session model).
- Single-file bind mounts vs atomic renames: verify claude's write pattern
  for `~/.claude.json` and pi's for `auth.json` before trusting file binds;
  fall back to `CLAUDE_CONFIG_DIR` / directory-level mounts if renames break
  them.
- The claude ephemeral denylist (`statsig/`, `todos/`, `shell-snapshots/`,
  `debug/`) is a best-current-guess; the teardown report exists to refine it.
- Should the container share the *host's* agent auth instead of keeping its
  own? Currently: own state, so a containerized agent never holds host
  tokens. Revisit only if double-login friction annoys.
- Firewall sidecar (long-planned) intersects with the session model — each
  session's compose stack is where it would attach. Out of scope here.
