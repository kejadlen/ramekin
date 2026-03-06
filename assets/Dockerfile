FROM node:24-trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    fd-find \
    git \
    ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/fdfind /usr/bin/fd

RUN npm install -g @mariozechner/pi-coding-agent

WORKDIR /workspace

ENTRYPOINT ["pi"]
