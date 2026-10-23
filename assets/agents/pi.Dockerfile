ARG BASE
FROM ${BASE}

# pi publishes under @earendil-works. The older @mariozechner scope still
# resolves and still carries the `latest` tag, so it looks current, but it's
# abandoned at 0.73.1 — far enough behind that host extensions fail to load
# against it (no ./compat export in its pi-ai).
RUN --mount=type=cache,target=/root/.npm \
    npm install -g @earendil-works/pi-coding-agent
