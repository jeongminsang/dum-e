.PHONY: help dev run check test build release install clean

# Default target
help:
	@echo "DUM-E Development Commands:"
	@echo "  make dev                 - Run interactive TUI"
	@echo "  make run ARGS=\"...\"      - Run CLI with arguments (e.g. make run ARGS=\"status\")"
	@echo "  make check               - Run cargo check (offline, locked)"
	@echo "  make test                - Run test suite (offline, locked)"
	@echo "  make build               - Build debug binary (offline, locked)"
	@echo "  make release             - Build release binary (offline, locked)"
	@echo "  make install             - Install dume binary to PATH locally"
	@echo "  make clean               - Clean target directory"

# Launch interactive TUI session
dev:
	cargo run -p dume-cli

# Run with custom arguments
run:
	cargo run -p dume-cli -- $(ARGS)

# Code verification rules from AGENTS.md
check:
	cargo check --workspace --locked --offline

test:
	cargo test --workspace --locked --offline

build:
	cargo build --locked --offline

release:
	cargo build --release --locked --offline -p dume-cli

install:
	cargo install --locked --path crates/dume-cli

clean:
	cargo clean
