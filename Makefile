SHELL=/bin/bash

.PHONY: help build release test fmt fmt-check lint audit gitleaks ci install-tools clean

.DEFAULT_GOAL := help

help:
	@echo "luggage - available commands:"
	@echo ""
	@echo "  make build         - Build (debug)"
	@echo "  make release       - Static release build for x86_64-unknown-linux-musl"
	@echo "  make test          - Run all tests"
	@echo "  make fmt           - Format code"
	@echo "  make fmt-check     - Check code formatting"
	@echo "  make lint          - Run clippy (warnings are errors)"
	@echo "  make audit         - Check dependencies for security advisories"
	@echo "  make gitleaks      - Scan git history and working tree for secrets"
	@echo "  make ci            - Run all CI checks (fmt-check, lint, test, audit, gitleaks)"
	@echo "  make install-tools - Install cargo-audit and gitleaks"
	@echo "  make clean         - Remove build artifacts"
	@echo ""
	@echo "audit and gitleaks fall back to 'nix run nixpkgs#...' when not installed."

build:
	cargo build

release:
	cargo build --release --target x86_64-unknown-linux-musl

test:
	cargo test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets -- -D warnings

audit:
	@if command -v cargo-audit >/dev/null; then cargo audit; else nix run nixpkgs#cargo-audit -- audit; fi

gitleaks:
	@if command -v gitleaks >/dev/null; then gl=gitleaks; else gl="nix run nixpkgs#gitleaks --"; fi; \
		$$gl git --no-banner . && $$gl dir --no-banner .

ci: fmt-check lint test audit gitleaks
	@echo "[ok] All CI checks passed!"

install-tools:
	cargo install cargo-audit --locked
	@echo "gitleaks: install via your package manager or 'nix profile install nixpkgs#gitleaks'"

clean:
	cargo clean
