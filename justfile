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
install:
    cargo install --path .

# Run all tests
test:
    cargo test

# Run tests with output
test-verbose:
    cargo test -- --nocapture

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

# Run all checks: format, lint, test, and build
check: fmt-check lint test build
    @echo "✓ All checks passed!"

# Clean build artifacts
clean:
    cargo clean

# Show project information
info:
    @echo "Gru - Local-First LLM Agent Orchestrator"
    @echo "Rust version: `rustc --version`"
    @echo "Cargo version: `cargo --version`"
