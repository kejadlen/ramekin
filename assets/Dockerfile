FROM node:24-trixie-slim

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    fd-find \
    git \
    jq \
    just \
    ripgrep \
    && ln -sf /usr/bin/fdfind /usr/bin/fd

RUN --mount=type=cache,target=/tmp/downloads \
    curl -fsSL "https://github.com/Wilfred/difftastic/releases/latest/download/difft-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin difft \
    && curl -fsSL "https://github.com/facebook/dotslash/releases/latest/download/dotslash-linux-musl.aarch64.tar.gz" \
      | tar xz -C /usr/local/bin dotslash \
    && curl -fsSL "https://github.com/kejadlen/ranger/releases/latest/download/ranger-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin ranger

RUN --mount=type=secret,id=github-token,target=/run/secrets/github-token \
    if [ -s /run/secrets/github-token ]; then \
      JJ_VERSION=$(curl -s -H "Authorization: Bearer $(cat /run/secrets/github-token)" \
        https://api.github.com/repos/jj-vcs/jj/releases/latest | jq -r .tag_name); \
    else \
      JJ_VERSION=$(curl -s https://api.github.com/repos/jj-vcs/jj/releases/latest | jq -r .tag_name); \
    fi && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj

RUN --mount=type=cache,target=/root/.npm \
    npm install -g @mariozechner/pi-coding-agent @anthropic-ai/claude-code

# Claude Code refuses --dangerously-skip-permissions when running as root
# unless IS_SANDBOX=1 acknowledges the sandbox. The container is the sandbox.
# Pi ignores this.
ENV IS_SANDBOX=1

# Run Claude in bypass-permissions mode and skip the bypass dialog. Managed
# settings is the highest tier, so this overrides whatever's in the user's
# mounted ~/.claude. defaultMode replaces the --dangerously-skip-permissions
# CLI flag; skipDangerousModePermissionPrompt suppresses the once-per-machine
# acceptance dialog. The container is already a sandbox (see IS_SANDBOX), so
# prompting is redundant.
RUN mkdir -p /etc/claude-code && cat > /etc/claude-code/managed-settings.json <<'EOF'
{
  "permissions": { "defaultMode": "bypassPermissions" },
  "skipDangerousModePermissionPrompt": true
}
EOF

WORKDIR /workspace

# No ENTRYPOINT: one image carries both agents, and the generated compose
# config picks `pi` or `claude` per session.
