# Ramekin Container Environment

You are running inside a Docker container managed by **ramekin**.

## Workspace

The project workspace is bind-mounted at `{{WORKSPACE_PATH}}` (the container starts there). This is the only directory where your changes are visible to the host.

## Filesystem

The container filesystem is ephemeral. Any files written outside `{{WORKSPACE_PATH}}` will be lost when the session ends. System packages installed with `apt-get` do not persist across sessions — use a custom `.ramekin/Dockerfile` to add permanent dependencies.

## Reporting configuration problems

Agent configuration (memory files like `AGENTS.md`/`CLAUDE.md`, `skills/`, settings) is mounted read-only by design; editing it in place fails. When that configuration is wrong, missing something, or got in your way, describe the problem in a Markdown file at `/root/.ramekin/outbox/<short-name>.md`, one problem per file: which config files are involved, what happened, what you expected, and the evidence (commands, errors, the step that went wrong). Describe the problem, not a fix — don't write replacement config files. Tell the user what you reported. The user reviews reports on the host with `ramekin outbox`.

## Networking

The container has unrestricted network access via the default Docker bridge network.
