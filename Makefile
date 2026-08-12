# Dray — developer entry points.
#
# Everything a reviewer needs is here. If a command is not in this file, it is
# not part of the documented workflow.

SHELL := /bin/bash
.DEFAULT_GOAL := help

CARGO ?= cargo
COMPOSE ?= docker compose

# Proving toolchain versions. Pinned deliberately — see ADR-002. Noir is
# pre-1.0 and its interchange format with the backend is not stable, so nargo
# and bb must be upgraded together and the Solidity verifiers regenerated.
NOIR_VERSION ?= 1.0.0-beta.22
BB_VERSION ?= 5.0.0-nightly.20260522
FOUNDRY_VERSION ?= 1.7.1

.PHONY: help setup setup-zk versions build test lint fmt up down clean \
        circuits contracts prove e2e-circuits e2e

help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

setup: ## Install toolchain components and verify prerequisites
	@command -v $(CARGO) >/dev/null || { \
		echo "cargo not found. Install Rust: https://rustup.rs"; \
		echo "If rustup is already installed, run: source \$$HOME/.cargo/env"; \
		exit 1; }
	@command -v cc >/dev/null || { \
		echo "no C linker found. Install one: sudo apt-get install -y build-essential"; \
		exit 1; }
	rustup component add rustfmt clippy
	$(CARGO) fetch
	@echo
	@echo "Rust toolchain ready. For circuits and contracts, run: make setup-zk"

setup-zk: ## Install the pinned Noir, Barretenberg, and Foundry toolchain
	@command -v noirup >/dev/null || \
		curl -fsSL https://raw.githubusercontent.com/noir-lang/noirup/main/install | bash
	@command -v bbup >/dev/null || \
		curl -fsSL https://raw.githubusercontent.com/AztecProtocol/aztec-packages/master/barretenberg/bbup/install | bash
	@command -v foundryup >/dev/null || \
		curl -fsSL https://foundry.paradigm.xyz | bash
	PATH="$$HOME/.nargo/bin:$$HOME/.bb:$$HOME/.foundry/bin:$$PATH" noirup -v $(NOIR_VERSION)
	PATH="$$HOME/.nargo/bin:$$HOME/.bb:$$HOME/.foundry/bin:$$PATH" bbup -v $(BB_VERSION)
	PATH="$$HOME/.nargo/bin:$$HOME/.bb:$$HOME/.foundry/bin:$$PATH" foundryup -i v$(FOUNDRY_VERSION)
	@echo
	@echo "Add to your shell profile:"
	@echo '  export PATH="$$HOME/.nargo/bin:$$HOME/.bb:$$HOME/.foundry/bin:$$PATH"'

versions: ## Print the versions of every tool this project depends on
	@echo "expected: nargo $(NOIR_VERSION), bb $(BB_VERSION), foundry $(FOUNDRY_VERSION)"
	@echo -n "actual:   nargo "; nargo --version 2>/dev/null | grep -oP '(?<=nargo version = ).*' || echo "(not installed)"
	@echo -n "          bb    "; bb --version 2>/dev/null || echo "(not installed)"
	@echo -n "          forge "; forge --version 2>/dev/null | grep -oP '(?<=forge Version: ).*' || echo "(not installed)"
	@echo -n "          cargo "; $(CARGO) --version 2>/dev/null | grep -oP '(?<=cargo ).*' || echo "(not installed)"

build: ## Build the whole workspace
	$(CARGO) build --workspace --all-targets

test: ## Run the test suite
	$(CARGO) test --workspace --all-targets

lint: ## Check formatting and run clippy with warnings as errors
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: ## Format the workspace in place
	$(CARGO) fmt --all

up: ## Start Postgres and Redis, waiting for both to be healthy
	$(COMPOSE) up -d --wait

down: ## Stop dependencies, keeping volumes
	$(COMPOSE) down

clean: ## Stop dependencies and delete their volumes
	$(COMPOSE) down -v

circuits: ## Compile circuits and run their Noir tests
	cd circuits && nargo compile && nargo test

contracts: ## Build contracts and run the Foundry suite (needs proofs; run make prove)
	cd contracts && forge build && forge test

prove: ## Generate proofs and Solidity verifiers for every circuit
	@bash scripts/prove.sh

e2e-circuits: ## Phase 1 end-to-end: input -> proof -> on-chain verification on Anvil
	@bash scripts/e2e-circuits.sh

e2e: ## End-to-end: API through to on-chain settlement (Phase 4)
	@echo "make e2e is not implemented yet — it lands in Phase 4."
	@echo "See PROGRESS.md for current phase status."
	@exit 1
