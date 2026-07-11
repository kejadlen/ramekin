# Ramekin Container Environment

You are running inside a Docker container managed by **ramekin**.

## Workspace

The project workspace is bind-mounted at `{{WORKSPACE_PATH}}` (the container starts there). This is the only directory where your changes are visible to the host.

## Filesystem

The container filesystem is ephemeral. Any files written outside `{{WORKSPACE_PATH}}` will be lost when the session ends. System packages installed with `apt-get` do not persist across sessions — use a custom `.ramekin/Dockerfile` to add permanent dependencies.

Agent configuration (`AGENTS.md`, `skills/`, and the like) is mounted read-only by design; edits to it inside the container fail. If a config change seems worthwhile, tell the user about it instead.

## Networking

The container has unrestricted network access via the default Docker bridge network.
