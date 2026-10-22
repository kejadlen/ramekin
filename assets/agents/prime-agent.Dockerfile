ARG BASE
FROM ${BASE}

# prime-agent's Python kernel venv defaults to ~/.prime/agent/kernel-venv,
# which the session mounts shadow, so it would re-bootstrap from the network
# every run. Moving it into the image lets the installer's pre-warm stick.
ENV PRIME_AGENT_KERNEL_VENV=/opt/prime-agent/kernel-venv
RUN curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh \
      | PRIME_AGENT_RUST_PREFIX=/usr/local PRIME_AGENT_UV_BIN_DIR=/usr/local/bin sh
