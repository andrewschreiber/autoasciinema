# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands
- `cargo build` - Build in debug mode
- `cargo build --release` - Build optimized release version
- `ASCIINEMA_GEN_DIR=/path cargo build --release` - Generate man pages and shell completions

## Test Commands
- `cargo test` - Run all tests
- `cargo test -- --nocapture` - Run tests with output visible
- `cargo test <test_name>` - Run specific test
- `cargo test <module>::<test_name>` - Run specific test in a module

## Lint & Format Commands
- `cargo fmt` - Format code according to project standards
- `cargo clippy` - Run linting checks

## Code Style Guidelines
- Follow standard Rust conventions
- Use `rustfmt` for consistent formatting
- Resolve all clippy warnings
- Use meaningful variable and function names
- Implement proper error handling with `anyhow`
- Import organization: group standard library, external crates, then internal modules
- Use Rust 2021 edition features
- MSRV (Minimum Supported Rust Version): 1.75.0
- Windows is not currently supported