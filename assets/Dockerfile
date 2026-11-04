FROM node:24-trixie-slim

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update && apt-get install -y --no-install-recommends \
    bc \
    build-essential \
    ca-certificates \
    curl \
    fd-find \
    git \
    jq \
    just \
    ripgrep \
    && ln -sf /usr/bin/fdfind /usr/bin/fd

# difftastic 0.71.0 started stamping the version into its asset names, so
# /releases/latest/download/ no longer resolves and the tag has to be looked up.
RUN --mount=type=cache,target=/tmp/downloads \
    --mount=type=secret,id=github-token,target=/run/secrets/github-token \
    if [ -s /run/secrets/github-token ]; then \
      DIFFT_VERSION=$(curl -s -H "Authorization: Bearer $(cat /run/secrets/github-token)" \
        https://api.github.com/repos/Wilfred/difftastic/releases/latest | jq -r .tag_name); \
    else \
      DIFFT_VERSION=$(curl -s https://api.github.com/repos/Wilfred/difftastic/releases/latest | jq -r .tag_name); \
    fi \
    && curl -fsSL "https://github.com/Wilfred/difftastic/releases/download/${DIFFT_VERSION}/difft-${DIFFT_VERSION}-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin difft \
    && curl -fsSL "https://github.com/facebook/dotslash/releases/latest/download/dotslash-linux-musl.aarch64.tar.gz" \
      | tar xz -C /usr/local/bin dotslash \
    && curl -fsSL "https://github.com/kejadlen/ranger/releases/latest/download/ranger-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin ranger \
    && curl -fsSL "https://github.com/zizmorcore/zizmor/releases/latest/download/zizmor-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin zizmor

RUN --mount=type=secret,id=github-token,target=/run/secrets/github-token \
    if [ -s /run/secrets/github-token ]; then \
      JJ_VERSION=$(curl -s -H "Authorization: Bearer $(cat /run/secrets/github-token)" \
        https://api.github.com/repos/jj-vcs/jj/releases/latest | jq -r .tag_name); \
    else \
      JJ_VERSION=$(curl -s https://api.github.com/repos/jj-vcs/jj/releases/latest | jq -r .tag_name); \
    fi && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj

# Trixie's apt git (2.47) lags upstream. Build the latest stable tag from
# source into /usr/local, which shadows /usr/bin/git via PATH. The apt git
# stays installed so its runtime libraries (libcurl, libexpat, zlib) are
# present; git/git has no /releases/latest, so resolve via ls-remote.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    --mount=type=cache,target=/tmp/downloads \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update && apt-get install -y --no-install-recommends \
    libcurl4-openssl-dev \
    libexpat1-dev \
    zlib1g-dev \
    && GIT_VERSION=$(git ls-remote --tags https://github.com/git/git.git 'v*' \
      | grep -v '\^{}' | grep -vE -- '-rc|-windows' | cut -d/ -f3 | sort -V | tail -1) \
    && curl -fsSL -o /tmp/downloads/git.tar.gz \
      "https://github.com/git/git/archive/refs/tags/${GIT_VERSION}.tar.gz" \
    && tar xzf /tmp/downloads/git.tar.gz -C /tmp \
    && cd /tmp/git-${GIT_VERSION#v} \
    && make prefix=/usr/local NO_GETTEXT=1 NO_TCLTK=1 -j"$(nproc)" all \
    && make prefix=/usr/local NO_GETTEXT=1 NO_TCLTK=1 install \
    && rm -rf /tmp/git-${GIT_VERSION#v} \
    && git --version

# pi publishes under @earendil-works. The older @mariozechner scope still
# resolves and still carries the `latest` tag, so it looks current, but it's
# abandoned at 0.73.1 — far enough behind that host extensions fail to load
# against it (no ./compat export in its pi-ai).
RUN --mount=type=cache,target=/root/.npm \
    npm install -g @earendil-works/pi-coding-agent @anthropic-ai/claude-code

# Claude Code refuses --dangerously-skip-permissions when running as root
# unless IS_SANDBOX=1 acknowledges the sandbox. The container is the sandbox.
# Pi ignores this.
ENV IS_SANDBOX=1

# Tells host config mounted into the container (extensions, hooks) that it's
# inside ramekin. Docker's /.dockerenv doesn't exist under the apple runtime.
ENV RAMEKIN=1

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
