# Ramekin Container Environment

You are running inside a Docker container managed by **ramekin**.

## Workspace

The project workspace is bind-mounted at `{{WORKSPACE_PATH}}` (the container starts there). This is the only directory where your changes are visible to the host.

## Filesystem

The container filesystem is ephemeral. Any files written outside `{{WORKSPACE_PATH}}` will be lost when the session ends. System packages installed with `apt-get` do not persist across sessions — use a custom `.ramekin/Dockerfile` to add permanent dependencies.

## Proposing configuration changes

Agent configuration (memory files like `AGENTS.md`/`CLAUDE.md`, `skills/`, settings) is mounted read-only by design; editing it in place fails. To propose a change, write the complete updated file into `/root/.ramekin/outbox/`, mirroring its layout relative to your config directory (for example, a change to `skills/foo/SKILL.md` goes to `/root/.ramekin/outbox/skills/foo/SKILL.md`), and tell the user what you proposed and why. The user reviews and applies proposals on the host with `ramekin outbox`.

## Networking

The container has unrestricted network access via the default Docker bridge network.
