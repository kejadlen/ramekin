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
    curl -fsSL "https://github.com/facebook/dotslash/releases/latest/download/dotslash-linux-musl.aarch64.tar.gz" \
      | tar xz -C /usr/local/bin dotslash \
    && curl -fsSL "https://github.com/kejadlen/ranger/releases/latest/download/ranger-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin ranger

# jj and difftastic put the version in their asset names, so the latest tag
# has to be resolved through the API first.
RUN --mount=type=secret,id=github-token,target=/run/secrets/github-token \
    latest() { \
      if [ -s /run/secrets/github-token ]; then \
        curl -s -H "Authorization: Bearer $(cat /run/secrets/github-token)" \
          "https://api.github.com/repos/$1/releases/latest" | jq -r .tag_name; \
      else \
        curl -s "https://api.github.com/repos/$1/releases/latest" | jq -r .tag_name; \
      fi; \
    } && \
    JJ_VERSION=$(latest jj-vcs/jj) && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj && \
    DIFFT_VERSION=$(latest Wilfred/difftastic) && \
    curl -fsSL "https://github.com/Wilfred/difftastic/releases/download/${DIFFT_VERSION}/difft-${DIFFT_VERSION}-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin difft

WORKDIR /workspace

# No agent here: each agent installs in its own layer on top of this image,
# or on top of a project image built from it. See assets/agents/.
