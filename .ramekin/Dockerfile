FROM ramekin-agent

ENV RANGER_DEFAULT_BACKLOG="ramekin"

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
