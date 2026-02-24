default: check fmt clippy

check:
    cargo check --workspace

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace

cov:
    cargo llvm-cov --workspace --html
    @echo "Coverage report: target/llvm-cov/html/index.html"
