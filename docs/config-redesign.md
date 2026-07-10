# Config redesign

Status: proposal (2026-07), revised against the `claude-code` branch

## Goals

1. **Multiple agents** — run either pi or Claude Code in the container,
   selected per project or per run.
2. **Shareable config** — the interesting parts of my setup (memory files,
   skills, mounts, agent choice) live in one place (dotfiles) and are usable
   from any machine and any project.
3. **Overridable everything** — any shared setting can be overridden closer
   to the point of use: per machine, per project, per run.
4. **Safe outbound sharing** — when the agent improves a skill or memory file
   inside the container, there's a safe path for that change to land back in
   the shared source of truth.

## Baseline: what `claude-code` already delivers

The `claude-code` branch covers goal 1 end to end, and its choices supersede
an earlier draft of this doc:

- `agent "pi" | "claude"` as a top-level KDL scalar; highest-precedence layer
  wins, defaulting to pi. `AgentLayout` in `main.rs` carries each agent's
  paths, builtin mounts, embedded Dockerfile, and prompt plumbing.
- **Bind mounts, not copies, for claude.** `claude { ... }` entries expand to
  read-only-by-default bind mounts at `/root/.claude/<target>`. Host edits
  are live in the container; auth and runtime state in the surrounding
  `~/.claude` mount are untouched; no clear-and-reassemble ceremony. The
  `pi { ... }` copy-assembly model stays as-is — the asymmetry is deliberate,
  matching how each agent treats its config dir.
- **Per-slug workspace instead of a sessions mount.** Claude keys its
  `projects` map and transcripts by cwd, so mounting each workspace at
  `/workspace/<slug>` isolates repos while `~/.claude` and `~/.claude.json`
  stay global (auth, identity, onboarding survive repo switches).
- Yolo mode via managed settings baked into the image
  (`bypassPermissions` + skip dialog + `IS_SANDBOX=1`), not CLI flags.
- Separate `Dockerfile.claude`, GitHub token forwarded as a BuildKit secret,
  side-effect-free `ramekin config`, deterministic parent-before-child mount
  ordering.

### Loose ends on the branch itself

- **Rebase onto main.** `claude-code` forked before the last five main
  commits (project image tags, `--rebuild` semantics, file-source mounts,
  justfile CI). README.md and src/main.rs conflict; the project-image-tag
  work on main overlaps with the shared `ramekin-agent` tag assumption below.
- **Shared base tag across agents.** Both Dockerfiles build to
  `ramekin-agent`, and project Dockerfiles say `FROM ramekin-agent`.
  Switching agents silently swaps what that tag means and busts every
  project layer built on the other agent. Per-agent tags
  (`ramekin-agent-pi`, `ramekin-agent-claude`) with the project `FROM`
  rewritten or parametrized via build arg would fix the churn.
- **README drift.** The branch README says claude gets the prompt via
  `--append-system-prompt`, but the code passes `--append-system-prompt-file`;
  the pi section describes a `ramekin.ts` extension while the code writes
  `ramekin-prompt.md` and passes a flag. Reconcile during the rebase.

## Remaining work

### 1. Sharing in: `include`

Any config file may include others:

```kdl
// ~/.config/ramekin/config.kdl — machine-specific, tiny
include "~/.dotfiles/ramekin/config.kdl"   // the shared base

// machine-only additions/overrides below
mounts {
    source "~/.local/share/ranger"
    writable
}
```

Rules:

- Included files load as their own layer at *lower* precedence than the
  includer — the machine file overrides the shared base.
- Includes may nest; cycles are an error. A missing include is an error too:
  unlike mount sources (where absence is a host fact), a dangling include
  means the config is wrong.
- `include` accepts a file or a directory (loads `*.kdl` sorted by name).

The shared config becomes a plain directory in dotfiles, versioned with jj.
New machine setup is one line of user config. This composes with the
`claude {}` / `pi {}` blocks already on the branch — the shared file declares
the dotfiles-sourced entries once, for both agents.

### 2. Overriding: two more layers

Layer order, lowest to highest precedence:

1. **builtin defaults** (agent = pi; builtin mounts stay non-overridable)
2. **included files**, in include order
3. **user** `~/.config/ramekin/config.kdl`
4. **project** `<workspace>/.ramekin/config.kdl`
5. **project-local** `<workspace>/.ramekin/config.local.kdl` — gitignored,
   for things true of this checkout on this machine only
6. **CLI** — at minimum `--agent`; `--mount` and `--env` when needed

Merge semantics stay exactly what the branch has: dedupe by resolved target
or name, scalars last-writer-wins, `/dev/null` masking to remove an inherited
mount. `ramekin config` already labels scopes; it grows the new ones.

`--agent` is the piece with real pull: today comparing pi and claude on the
same repo means editing a config file back and forth. (It also interacts
with the shared-base-tag loose end above — per-run agent switching makes the
tag churn much more visible.)

### 3. Sharing out: jj-backed writable mounts now, outbox if needed

The branch already contains most of the answer. A `claude {}` entry with
`writable` bind-mounts a file or directory out of the dotfiles *working
copy* — and crucially, only that path, never the repo metadata. That gives:

- The agent can write improvements directly (skills, CLAUDE.md).
- The agent cannot touch `.jj`/`.git`, other dotfiles, or history.
- Every change lands as an ordinary working-copy diff on the host: `jj st`
  shows it, `jj diff` reviews it, `jj restore` rejects it. Nothing is
  irreversible.

So the safety model is *review-after with guaranteed rollback*, which for a
single user is probably the right cost/benefit. Two cheap hardening steps
make it trustworthy enough to leave on:

- A `ramekin-prompt.md` section telling the agent which paths are shared
  config and that edits there propagate to the host — so changes are
  deliberate, not incidental.
- `ramekin run` warns at startup when a writable mount's source sits in a
  dirty jj working copy, so agent edits don't get tangled with unrelated
  uncommitted changes.

If review-*before* ever becomes necessary (or for the pi side, where config
is copied and in-container edits currently evaporate), the fallback design is
an **outbox**: a fresh per-session writable mount at `/root/.ramekin/outbox`,
a prompt instruction to drop proposed config changes there mirroring the
config-dir layout, and a `ramekin outbox list|diff|apply|discard` subcommand
that diffs proposals against their known sources and copies them over only on
explicit apply. Nothing reaches dotfiles without confirmation. The outbox is
strictly additive — same mount plumbing, no changes to the models above — so
deferring it costs nothing.

## Sequencing

1. Rebase `claude-code` onto main, fixing the README drift and deciding the
   base-tag question in the same pass. Merge it — it's the foundation.
2. `include` + project-local layer (config.rs only, mechanical).
3. `--agent` CLI override; `--mount`/`--env` if they earn their keep.
4. Prompt section + dirty-working-copy warning for writable shared mounts.
5. Outbox, only if review-before turns out to matter in practice.

## Open questions

- Per-agent base image tags vs. one shared tag (see loose ends). Leaning
  per-agent tags with the project Dockerfile `FROM` parametrized by build arg.
- Should pi eventually move to the bind-mount model (a `writable` field on
  `pi {}` entries) so both agents share outbound semantics? Today pi's
  copy-assembly means in-container edits are always lost, which makes the
  outbox more interesting for pi than for claude.
