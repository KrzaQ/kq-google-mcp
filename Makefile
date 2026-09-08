.PHONY: help install install-backend install-frontend format lint test \
	build build-frontend dev-backend dev-frontend types image docker-build docker-run clean
.DEFAULT_GOAL := help

# Machine-specific targets (deploy, extra push remotes) live in Makefile.local,
# which is gitignored. See Makefile.local.example.

# Dev backend: no authentik, one fixed user, loopback only. The server refuses
# GMCP_AUTH=dev on any other public URL. .env is read when present so a real
# Google client can be used against the localhost redirect URI.
DEV_PORT ?= 8000
DEV_PUBLIC_URL ?= http://localhost:$(DEV_PORT)
DEV_DATABASE ?= ./data/gmcp.db

help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "; w = 12} { k[NR] = $$1; v[NR] = $$2; if (length($$1) > w) w = length($$1) } \
		     END { for (i = 1; i <= NR; i++) printf "  \033[36m%-*s\033[0m %s\n", w, k[i], v[i] }'

install: install-backend install-frontend ## Install all dependencies

install-backend: ## Fetch Rust dependencies
	cargo fetch

install-frontend: ## Install frontend dependencies
	cd frontend && npm ci

format: ## Format code
	cargo fmt
	cd frontend && npm run format

lint: ## Run linters and type checks
	cargo clippy --all-targets -- -D warnings
	cd frontend && npm run lint && npm run typecheck

test: ## Run the test suites (no database, no network)
	cargo test
	cd frontend && npm test

build-frontend: install-frontend ## Build the frontend bundle
	cd frontend && npm run build

build: install format lint build-frontend ## Install, format, lint, then build the release binary
	cargo build --release

dev-backend: ## Run the backend in dev-auth mode on $(DEV_PORT)
	if [ -f .env ]; then set -a; . ./.env; set +a; fi; \
		GMCP_AUTH=dev GMCP_PUBLIC_URL=$(DEV_PUBLIC_URL) GMCP_BIND=127.0.0.1:$(DEV_PORT) \
		GMCP_DATABASE=$(DEV_DATABASE) cargo run -- serve

dev-frontend: ## Run the Vite dev server (proxies /api to the dev backend)
	cd frontend && npm run dev

types: ## Regenerate frontend/src/api/schema.d.ts from a running dev backend
	cd frontend && npm run types

docker-build: ## Build the deployable image (gmcp:local)
	docker compose build

image: docker-build ## Build the image and tag it with the commit, for deploying elsewhere
	docker tag gmcp:local gmcp:$$(git rev-parse --short HEAD)

docker-run: docker-build ## Build and run the stack from this checkout
	docker compose up

clean: ## Remove build artifacts
	cargo clean
	rm -rf frontend/node_modules frontend/dist

-include Makefile.local
