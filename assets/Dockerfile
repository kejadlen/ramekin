FROM node:24-trixie-slim

ARG TARGETARCH

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

ARG DOTSLASH_VERSION=0.5.8
RUN case "${TARGETARCH}" in \
      amd64) DS_ARCH="x86_64" ;; \
      arm64) DS_ARCH="aarch64" ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" && exit 1 ;; \
    esac && \
    curl -fsSL "https://github.com/facebook/dotslash/releases/download/v${DOTSLASH_VERSION}/dotslash-linux-musl.${DS_ARCH}.tar.gz" \
      | tar xz -C /usr/local/bin dotslash

ARG DIFFTASTIC_VERSION=0.68.0
RUN case "${TARGETARCH}" in \
      amd64) DFT_TARGET="x86_64-unknown-linux-musl" ;; \
      arm64) DFT_TARGET="aarch64-unknown-linux-gnu" ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" && exit 1 ;; \
    esac && \
    curl -fsSL "https://github.com/Wilfred/difftastic/releases/download/${DIFFTASTIC_VERSION}/difft-${DFT_TARGET}.tar.gz" \
      | tar xz -C /usr/local/bin difft

ARG JJ_VERSION=0.39.0
RUN case "${TARGETARCH}" in \
      amd64) JJ_ARCH="x86_64" ;; \
      arm64) JJ_ARCH="aarch64" ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" && exit 1 ;; \
    esac && \
    curl -fsSL "https://github.com/jj-vcs/jj/releases/download/v${JJ_VERSION}/jj-v${JJ_VERSION}-${JJ_ARCH}-unknown-linux-musl.tar.gz" \
      | tar xz --strip-components=0 -C /usr/local/bin ./jj

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

RUN cargo install --git https://github.com/kejadlen/ranger.git --locked

RUN jj config set --user user.name "ramekin" && \
    jj config set --user user.email "ramekin@example.com"

RUN npm install -g @mariozechner/pi-coding-agent

WORKDIR /workspace

ENTRYPOINT ["pi"]
