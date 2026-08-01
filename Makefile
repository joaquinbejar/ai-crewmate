# Makefile for ai-crew-sync — MCP coordination bus for AI coding agent teams.
#
# Everything that reaches a real environment is explicit and named as such:
# `deploy` deploys to a swarm, `up`/`down` act on the local compose stack,
# and images are published by CI on version tags — nothing here pushes one.
# `make check` is the pre-push gate and runs offline; `make test` needs
# docker for its throwaway Postgres.

# --project-directory keeps the repo root as the project dir, so compose
# reads ./.env and names the project from COMPOSE_PROJECT_NAME there even
# though the files live under Docker/.
COMPOSE      = docker compose --project-directory . -f Docker/docker-compose.yml
COMPOSE_DEV  = $(COMPOSE) -f Docker/docker-compose.dev.yml
STACK       ?= crew

TEST_PG_NAME  = ai-crew-sync-test-pg
TEST_PG_PORT ?= 55432
TEST_PG_IMAGE ?= postgres:18-alpine

.DEFAULT_GOAL := all

.PHONY: all
all: check ## Default: the pre-push gate

.PHONY: help
help: ## List every target
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

# --- verification -----------------------------------------------------------

.PHONY: check
check: fmt-check lint validate config-check hooks-check ## Pre-push gate: format, clippy, compose renders, config documented, hooks sane. Offline.

.PHONY: pre-push
pre-push: check ## Alias for check

.PHONY: fmt
fmt: ## Format the Rust sources
	cargo fmt

.PHONY: fmt-check
fmt-check: ## Fail if a source file is unformatted
	cargo fmt --check

.PHONY: lint
lint: ## Clippy with warnings as errors, all targets
	cargo clippy --all-targets -- -D warnings

.PHONY: lint-fix
lint-fix: ## Apply the clippy fixes that are machine-applicable
	cargo clippy --fix --all-targets --allow-dirty -- -D warnings

.PHONY: validate
validate: ## Render both compose files (shape check; every variable has a default)
	@$(COMPOSE) config -q && echo "  ok  Docker/docker-compose.yml"
	@$(COMPOSE_DEV) config -q && echo "  ok  + Docker/docker-compose.dev.yml"

# A knob nobody can discover is a knob nobody can set. Every variable the
# server, the compose files, the client or the plugin reads must appear in
# .env.example — commented out is fine, absent is not. (Make's own variables
# are not user configuration; they are documented by `make help`.)
.PHONY: config-check
config-check: ## Fail if a configuration variable is missing from .env.example
	@missing=""; \
	for v in $$(grep -rhoE '\b(BUS|POSTGRES|DATABASE|RUST)_[A-Z_]+' \
			src Docker plugin 2>/dev/null | sort -u); do \
		case "$$v" in POSTGRES_USER|POSTGRES_DB) continue ;; esac; \
		grep -qE "^#? *$$v([=:]|$$)" .env.example || missing="$$missing $$v"; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "undocumented in .env.example:$$missing"; exit 1; \
	fi; \
	echo "config: every variable is documented"

.PHONY: hooks-check
hooks-check: ## Plugin hook regression tests (no bus, no network)
	@sh plugin/scripts/test-hooks.sh

.PHONY: test
test: ## Integration tests against a throwaway Postgres (needs docker)
	@docker rm -f $(TEST_PG_NAME) >/dev/null 2>&1 || true
	@docker run -d --name $(TEST_PG_NAME) -p $(TEST_PG_PORT):5432 \
		-e POSTGRES_USER=test -e POSTGRES_PASSWORD=test -e POSTGRES_DB=test \
		$(TEST_PG_IMAGE) >/dev/null
	@until docker exec $(TEST_PG_NAME) pg_isready -U test >/dev/null 2>&1; do sleep 1; done
	@TEST_DATABASE_URL=postgres://test:test@localhost:$(TEST_PG_PORT)/test cargo test; \
		status=$$?; docker rm -f $(TEST_PG_NAME) >/dev/null; exit $$status

# --- build and run ----------------------------------------------------------

.PHONY: build
build: ## Release build of the binary
	cargo build --release

.PHONY: run
run: ## Run the server from source against $$DATABASE_URL
	cargo run -- serve

# --- the stack --------------------------------------------------------------

.PHONY: up
up: ## Start the local stack (published image), detached
	$(COMPOSE) up -d

.PHONY: up-dev
up-dev: ## Start the local stack built from this checkout
	$(COMPOSE_DEV) up -d --build

.PHONY: down
down: ## Stop the stack (volumes are kept)
	$(COMPOSE) down

.PHONY: restart
restart: down up ## Restart the stack

.PHONY: ps
ps: ## Show stack state
	$(COMPOSE) ps

.PHONY: logs
logs: ## Follow stack logs
	$(COMPOSE) logs -f

.PHONY: deploy
deploy: ## Deploy to the current Docker Swarm as stack '$(STACK)' -- REACHES A REAL ENVIRONMENT
	docker stack deploy -c Docker/docker-compose.yml $(STACK)

# --- housekeeping -----------------------------------------------------------

.PHONY: clean
clean: ## Stop the stack and remove its volumes -- DESTROYS LOCAL DATA
	$(COMPOSE) down -v

.PHONY: dsstore
dsstore: ## Delete stray .DS_Store files
	@find . -name '.DS_Store' -type f -delete 2>/dev/null || true
