---
name: ramekin
description: Use when working inside a ramekin container — proposing agent config or skill changes via the outbox, handling read-only config mounts, persisting dependencies across sessions, or debugging ephemeral-filesystem and bind-mount issues.
---

# Ramekin containers

Ramekin runs coding agents (pi or Claude Code) inside Docker
containers. The project workspace is bind-mounted at
`/workspace/<dirname>-<hash>` and the container starts there. Agent
config — memory files, `skills/`, settings — mounts read-only at the
agent's normal path: `~/.pi/agent` for pi, `~/.claude` for claude.

*This is a self-improving skill. If you used it and it came up short — a
missing command, path, gotcha, or workflow — invoke the
`self-improving-skills` skill and follow it before you finish.*

## Proposing config changes (the outbox)

Editing anything under the read-only config mount fails. To propose a
change instead:

1. Write the **complete updated file** (not a diff) into
   `/root/.ramekin/outbox/`, mirroring its layout relative to the agent
   config directory. Examples:
   - `~/.claude/skills/jj/SKILL.md` → `/root/.ramekin/outbox/skills/jj/SKILL.md`
   - a new skill → `/root/.ramekin/outbox/skills/<name>/SKILL.md`
2. Tell the user what you proposed and why. They review and apply on
   the host with `ramekin outbox`.

New files (e.g., a skill that doesn't exist yet) go through the same
path — the outbox is for creations as well as edits.

## Reviewing proposals on the host

```bash
ramekin outbox list                       # pending proposals across all repos
ramekin outbox diff [<slug>/<session>[/<path>]]   # against the host baseline
ramekin outbox apply <slug>/<session>/<path>
ramekin outbox discard <slug>/<session>[/<path>]   # one proposal or a whole session
```

`apply` shows the diff, asks for confirmation, then copies the proposal
**over** the host file, discarding anything the host gained since the
container mounted its baseline. Proposals sit in the outbox
indefinitely and go stale, so read that diff rather than confirming
reflexively: when the host has moved on, hand-merge the proposal's idea
instead, then `discard`.

`apply` resolves symlinks before writing, so a proposal for a path that
dotfiles symlink elsewhere lands in the dotfiles working copy rather
than replacing the link. A proposal that doesn't map back to an
allowlisted agent-config entry needs an explicit `--to <path>`.

## jj commits as the wrong identity

Path-scoped jj config doesn't survive the container. Two reasons, both
worth checking before assuming a third: entries in
`~/.config/jj/conf.d/` often symlink into a repo that isn't mounted, so
they dangle; and a `--when.repositories` scope keyed on host paths can't
match anyway, because the workspace mounts under `/workspace`. jj
silently falls back to whatever the base config says.

Fix per repo, from the **host**:

```bash
jj config set --repo user.name "..."
jj config set --repo user.email "..."
```

This reaches the container because `~/.config/jj` mounts read-only as a
staple, and jj resolves repo config through `.jj/repo/config-id` into
`~/.config/jj/repos/` — not through the `.jj/repo/config.toml` symlink,
which holds a host absolute path and does dangle inside the container.
Confirm the mount is present with `ramekin config`; staples are skipped
when the host path is missing.

Anything else in that same scope block is lost too. Check for
`templates.git_push_bookmark` and `signing.key` before relying on them.

## Ephemeral filesystem

Only the workspace bind mount and the agent's own persistent state
survive the session. Everything else is lost, so:

- `apt-get install` does not persist — add permanent dependencies to a
  custom `.ramekin/Dockerfile` instead.
- Toolchains installed at runtime (rustup, uv, …) need reinstalling
  each session, or a Dockerfile entry. Check `~/.cargo/bin` and the
  like before reinstalling — sometimes they survive within a host
  session.

## Customizing the image

`.ramekin/Dockerfile` in the workspace extends the base image, which
carries both agents plus Node.js, git, jj, ripgrep, fd, just, jq,
difftastic, dotslash, and ranger. The workspace is the build context,
so `COPY` works relative to the project root.

```dockerfile
FROM ramekin-agent
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-client && rm -rf /var/lib/apt/lists/*
```

The image is rebuilt on every `ramekin run`.

## Bind-mount gotchas

The workspace is a host bind mount: the **host** can run out of disk
while `df` inside the container shows plenty free on the overlay.
Symptom: writes to the workspace fail mysteriously. Keep build
artifacts on the container side — e.g.
`CARGO_TARGET_DIR=/tmp/<something>` — which also speeds up builds.

Better still, declare a `cache` in `.ramekin/config.kdl` and let ramekin
keep the directory across sessions:

```kdl
cache {
    target    # shadows the workspace's own target/
}
```

A bare node uses its name as the container path, and relative paths
resolve inside the workspace, so the build tool needs no configuring and
the host's build directory stays untouched.
