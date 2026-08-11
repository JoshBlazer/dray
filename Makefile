# Dray — developer entry points.
#
# Everything a reviewer needs is here. If a command is not in this file, it is
# not part of the documented workflow.

SHELL := /bin/bash
.DEFAULT_GOAL := help

CARGO ?= cargo
COMPOSE ?= docker compose

.PHONY: help setup build test lint fmt up down clean e2e

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
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

e2e: ## End-to-end: API through to on-chain settlement (Phase 4)
	@echo "make e2e is not implemented yet — it lands in Phase 4."
	@echo "See PROGRESS.md for current phase status."
	@exit 1
