FROM node:24-trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    fd-find \
    git \
    jq \
    just \
    ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/fdfind /usr/bin/fd

RUN curl -fsSL "https://github.com/Wilfred/difftastic/releases/latest/download/difft-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin difft

RUN curl -fsSL "https://github.com/facebook/dotslash/releases/latest/download/dotslash-linux-musl.aarch64.tar.gz" \
      | tar xz -C /usr/local/bin dotslash

RUN JJ_VERSION=$(curl -s https://api.github.com/repos/jj-vcs/jj/releases/latest | jq -r .tag_name) && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/${JJ_VERSION}/jj-${JJ_VERSION}-aarch64-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj

RUN curl -fsSL "https://github.com/kejadlen/ranger/releases/latest/download/ranger-aarch64-unknown-linux-gnu.tar.gz" \
      | tar xz -C /usr/local/bin ranger

RUN npm install -g @mariozechner/pi-coding-agent

WORKDIR /workspace

ENTRYPOINT ["pi"]
