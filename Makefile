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

# Install locations, stated explicitly rather than inferred.
#
# Foundry's installer resolves its directory as $XDG_CONFIG_HOME/.foundry when
# that variable is set, falling back to $HOME/.foundry otherwise. GitHub's
# runners set it, so leaving this implicit installs the toolchain somewhere
# other than where the PATH below looks — which is exactly how this broke in
# CI while working locally. Pinning FOUNDRY_DIR makes the two agree everywhere.
FOUNDRY_DIR ?= $(HOME)/.foundry
NARGO_BIN := $(HOME)/.nargo/bin
BB_BIN := $(HOME)/.bb
FOUNDRY_BIN := $(FOUNDRY_DIR)/bin
ZK_PATH := $(NARGO_BIN):$(BB_BIN):$(FOUNDRY_BIN)

.PHONY: help setup setup-zk versions build test test-integration test-proving test-worker test-relayer reset-test-db lint fmt up down clean seed api worker relayer \
        circuits contracts prove deploy e2e-circuits e2e

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
	@test -x "$(NARGO_BIN)/noirup" || \
		curl -fsSL https://raw.githubusercontent.com/noir-lang/noirup/main/install | bash
	@test -x "$(BB_BIN)/bbup" || \
		curl -fsSL https://raw.githubusercontent.com/AztecProtocol/aztec-packages/master/barretenberg/bbup/install | bash
	@test -x "$(FOUNDRY_BIN)/foundryup" || \
		curl -fsSL https://foundry.paradigm.xyz | FOUNDRY_DIR="$(FOUNDRY_DIR)" bash
	PATH="$(ZK_PATH):$$PATH" "$(NARGO_BIN)/noirup" -v $(NOIR_VERSION)
	PATH="$(ZK_PATH):$$PATH" "$(BB_BIN)/bbup" -v $(BB_VERSION)
	PATH="$(ZK_PATH):$$PATH" FOUNDRY_DIR="$(FOUNDRY_DIR)" "$(FOUNDRY_BIN)/foundryup" -i v$(FOUNDRY_VERSION)
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

DATABASE_URL ?= postgres://dray:dray@localhost:5432/dray
TEST_DATABASE_URL ?= postgres://dray:dray@localhost:5432/dray_test
REDIS_URL ?= redis://localhost:6379

seed: ## Register the circuits with their input schemas
	@# Falls back to the Postgres container when psql is not installed locally,
	@# so the quickstart needs only Docker and Rust as advertised.
	@if command -v psql >/dev/null 2>&1; then \
		psql "$(DATABASE_URL)" -v ON_ERROR_STOP=1 -f scripts/seed-circuits.sql; \
	else \
		echo "psql not found locally; seeding through the postgres container"; \
		$(COMPOSE) exec -T postgres psql -U dray -d dray -v ON_ERROR_STOP=1 \
			< scripts/seed-circuits.sql; \
	fi

api: ## Run the ingest API against the local dependencies
	DATABASE_URL="$(DATABASE_URL)" $(CARGO) run -p dray-api

worker: ## Run a proving worker against the local dependencies
	@# The worker prepares circuit artefacts at start-up, so it needs the ZK
	@# toolchain on PATH as well as a database. REDIS_URL is optional: without
	@# it the worker simply does not mirror lease state.
	PATH="$(ZK_PATH):$$PATH" DATABASE_URL="$(DATABASE_URL)" REDIS_URL="$(REDIS_URL)" \
		$(CARGO) run -p dray-worker

test-integration: ## Run the tests that need a live Postgres (requires make up)
	@# A separate database, because the integration tests register a circuit per
	@# test and would otherwise litter the development database with hundreds of
	@# them — which is exactly what happened the first time this was run.
	@$(COMPOSE) exec -T postgres psql -U dray -d dray -tAc \
		"SELECT 1 FROM pg_database WHERE datname='dray_test'" | grep -q 1 || \
		$(COMPOSE) exec -T postgres psql -U dray -d dray -c "CREATE DATABASE dray_test"
	DATABASE_URL="$(TEST_DATABASE_URL)" REDIS_URL="$(REDIS_URL)" \
		$(CARGO) test -p dray-store --features integration-tests
	DATABASE_URL="$(TEST_DATABASE_URL)" $(CARGO) test -p dray-api --features integration-tests

test-proving: ## Run the worker tests that shell out to a real nargo and bb (requires make setup-zk)
	@# No live database needed — these exercise the proving pipeline alone, and
	@# assert the worker reproduces the same nullifiers e2e-circuits settles.
	PATH="$(ZK_PATH):$$PATH" $(CARGO) test -p dray-worker --features proving-tests

relayer: ## Run a relayer against the local dependencies and a local chain
	@# DRAY_RELAYER_KEY and DRAY_SETTLEMENT have no defaults: a key cannot have
	@# one, and a relayer pointed at the wrong contract fails every job.
	PATH="$(ZK_PATH):$$PATH" DATABASE_URL="$(DATABASE_URL)" $(CARGO) run -p dray-relayer

test-relayer: ## Run the relayer tests against Anvil (needs make up, setup-zk and prove)
	@# Each test starts its own Anvil. They need proofs on disk, because they
	@# settle real ones rather than fixtures.
	@test -f circuits/target/membership/proof || { \
		echo "no proofs on disk. Run: make prove"; exit 1; }
	@$(COMPOSE) exec -T postgres psql -U dray -d dray -tAc \
		"SELECT 1 FROM pg_database WHERE datname='dray_test'" | grep -q 1 || \
		$(COMPOSE) exec -T postgres psql -U dray -d dray -c "CREATE DATABASE dray_test"
	PATH="$(ZK_PATH):$$PATH" DATABASE_URL="$(TEST_DATABASE_URL)" \
		$(CARGO) test -p dray-relayer --features integration-tests --test anvil -- --test-threads 2

test-worker: ## Run the whole-worker tests: load and chaos (needs make up and make setup-zk)
	@# These need a live Postgres *and* the proving toolchain, which is why they
	@# are separate from both test-integration and test-proving.
	@$(COMPOSE) exec -T postgres psql -U dray -d dray -tAc \
		"SELECT 1 FROM pg_database WHERE datname='dray_test'" | grep -q 1 || \
		$(COMPOSE) exec -T postgres psql -U dray -d dray -c "CREATE DATABASE dray_test"
	PATH="$(ZK_PATH):$$PATH" DATABASE_URL="$(TEST_DATABASE_URL)" REDIS_URL="$(REDIS_URL)" \
		$(CARGO) test -p dray-worker --features integration-tests --test lease_loop

reset-test-db: ## Drop and recreate the integration-test database
	$(COMPOSE) exec -T postgres psql -U dray -d dray -c "DROP DATABASE IF EXISTS dray_test"
	$(COMPOSE) exec -T postgres psql -U dray -d dray -c "CREATE DATABASE dray_test"

circuits: ## Compile circuits and run their Noir tests
	cd circuits && nargo compile && nargo test

contracts: ## Build contracts and run the Foundry suite (needs proofs; run make prove)
	cd contracts && forge build && forge test

prove: ## Generate proofs and Solidity verifiers for every circuit
	@bash scripts/prove.sh

deploy: ## Deploy the settlement stack. Set DRAY_RPC_URL, PRIVATE_KEY, DRAY_RELAYERS
	@test -n "$$PRIVATE_KEY" || { echo "PRIVATE_KEY must be set"; exit 1; }
	cd contracts && PATH="$(ZK_PATH):$$PATH" forge script script/Deploy.s.sol:Deploy \
		--rpc-url "$${DRAY_RPC_URL:-http://127.0.0.1:8545}" --broadcast

e2e-circuits: ## Phase 1 end-to-end: input -> proof -> on-chain verification on Anvil
	@bash scripts/e2e-circuits.sh

e2e: ## End-to-end: API through to on-chain settlement (Phase 4)
	@echo "make e2e is not implemented yet — it lands in Phase 4."
	@echo "See PROGRESS.md for current phase status."
	@exit 1
