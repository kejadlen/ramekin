default: check fmt clippy

check:
    cargo check

fmt:
    cargo fmt --all

clippy:
    cargo clippy

install:
    cargo install --locked --path .
