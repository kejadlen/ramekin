FROM node:24-trixie-slim

ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    fd-find \
    git \
    just \
    ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && ln -sf /usr/bin/fdfind /usr/bin/fd

ARG JJ_VERSION=0.39.0
RUN case "${TARGETARCH}" in \
      amd64) JJ_ARCH="x86_64" ;; \
      arm64) JJ_ARCH="aarch64" ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" && exit 1 ;; \
    esac && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/v${JJ_VERSION}/jj-v${JJ_VERSION}-${JJ_ARCH}-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj

RUN jj config set --user user.name "ramekin" && \
    jj config set --user user.email "ramekin@example.com"

RUN npm install -g @mariozechner/pi-coding-agent

WORKDIR /workspace

ENTRYPOINT ["pi"]
