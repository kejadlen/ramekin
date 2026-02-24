default: check fmt clippy

check:
    cargo check --workspace

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace
