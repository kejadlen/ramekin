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

RUN JJ_VERSION=$(curl -s https://api.github.com/repos/jj-vcs/jj/releases/latest | jq -r .tag_name) && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj

RUN --mount=type=cache,target=/root/.npm \
    npm install -g @mariozechner/pi-coding-agent

WORKDIR /workspace

ENTRYPOINT ["pi"]
