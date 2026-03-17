default: check fmt clippy test

check:
    cargo check --workspace

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace -- -D warnings

test:
    cargo test --workspace

install:
    cargo install --locked --path .
