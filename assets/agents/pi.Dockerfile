ARG BASE
FROM node:24-trixie-slim AS node
FROM ${BASE}

# pi runs on Node, which only pi needs, so it comes in here rather than in
# the base. Debian's own nodejs is too old for pi's engines constraint.
COPY --from=node /usr/local/bin/node /usr/local/bin/
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# pi publishes under @earendil-works. The older @mariozechner scope still
# resolves and still carries the `latest` tag, so it looks current, but it's
# abandoned at 0.73.1 — far enough behind that host extensions fail to load
# against it (no ./compat export in its pi-ai).
RUN --mount=type=cache,target=/root/.npm \
    npm install -g @earendil-works/pi-coding-agent
