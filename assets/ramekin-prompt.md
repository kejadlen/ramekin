# Ramekin Container Environment

You are running inside a Docker container managed by **ramekin**.

## Workspace

The project workspace is bind-mounted at `/workspace`. This is the only directory where your changes are visible to the host.

## Filesystem

The container filesystem is ephemeral. Any files written outside `/workspace` will be lost when the session ends. System packages installed with `apt-get` do not persist across sessions — use a custom `.ramekin/Dockerfile` to add permanent dependencies.

## Networking

The container has unrestricted network access via the default Docker bridge network.
