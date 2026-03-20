# Gru Project Justfile
# Common development and CI workflows

# Default recipe - runs when you type 'just' with no arguments
default:
    @just --list

# Build the project
build:
    cargo build

# Build with release optimizations
build-release:
    cargo build --release

# Install the binary locally
install: build-release
    cp target/release/gru ~/.cargo/bin/gru.tmp && mv ~/.cargo/bin/gru.tmp ~/.cargo/bin/gru

# Update binary
update:
    git pull && just install

# Run all tests (requires cargo-nextest: cargo install cargo-nextest)
test:
    cargo nextest run

# Run tests with output
test-verbose:
    cargo nextest run --no-capture

# Run clippy linter with warnings as errors
lint:
    cargo clippy --all-targets -- -D warnings

# Automatically fix clippy lints where possible
fix-clippy:
    cargo clippy --all-targets --fix --allow-dirty --allow-staged

# Format code
fmt:
    cargo fmt --all

# Check formatting without modifying files
fmt-check:
    cargo fmt --all -- --check

# Audit dependencies for known vulnerabilities (requires cargo-audit: cargo install cargo-audit)
audit:
    cargo audit

# Run all checks: format, lint, test, and build
check: fmt-check lint test build
    @echo "✓ All checks passed!"

# Clean build artifacts
clean:
    cargo clean

# Generate CHANGELOG.md from merged PRs between tags (requires: cargo install git-cliff)
changelog:
    git-cliff --github-repo fotoetienne/gru -o CHANGELOG.md

# Preview changelog without writing (requires: cargo install git-cliff)
changelog-preview:
    git-cliff --github-repo fotoetienne/gru

# Show project information
info:
    @echo "Gru - Local-First LLM Agent Orchestrator"
    @echo "Rust version: `rustc --version`"
    @echo "Cargo version: `cargo --version`"
