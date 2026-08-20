# hydrant — developer task runner.
# Run `make` or `make help` for the list of targets.
#
# Targets mirror the CI pipeline (.github/workflows/ci.yml) so every gate can be
# reproduced locally before pushing. CI calls these same targets, so the exact
# commands live in one place.

CARGO        ?= cargo
# CI sets CARGO_LOCKED=--locked so a stale Cargo.lock fails the build instead of
# being updated silently. Left empty locally, where updating it is the point.
CARGO_LOCKED ?=
DATABASE_URL ?= postgres://hydrant:hydrant@localhost:5433/hydrant
ARGS         ?=

export DATABASE_URL

.DEFAULT_GOAL := help

# ---------------------------------------------------------------------------
##@ General
# ---------------------------------------------------------------------------

.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

# ---------------------------------------------------------------------------
##@ Quality (-> quality-gate)
# ---------------------------------------------------------------------------

.PHONY: fmt
fmt: ## Format the whole workspace
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting (CI: Static Quality)
	$(CARGO) fmt --all --check

.PHONY: clippy
clippy: ## Lint with clippy, warnings as errors (CI: Clippy Lint)
	$(CARGO) clippy --workspace --all-targets --all-features $(CARGO_LOCKED) -- -D warnings

.PHONY: check
check: ## Type-check the workspace (CI: Build Check / MSRV)
	$(CARGO) check --workspace --all-targets $(CARGO_LOCKED)

.PHONY: doc
doc: ## Build docs, warnings as errors (CI: Static Quality)
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps $(CARGO_LOCKED)

.PHONY: quality
quality: fmt-check clippy check doc ## Run every quality gate

# ---------------------------------------------------------------------------
##@ Security (-> security-gate)
# ---------------------------------------------------------------------------

.PHONY: deny
deny: ## Advisories + bans + licenses + sources (CI: Cargo Deny)
	$(CARGO) deny check

.PHONY: security
security: deny ## Run every security gate

# ---------------------------------------------------------------------------
##@ Tests
# ---------------------------------------------------------------------------
#
# `--no-tests=pass` keeps the gate green while a crate has no tests yet. Drop it
# once the projection property tests land (M0) — after that, a workspace with no
# tests to run is a mistake worth failing on.

.PHONY: test
test: ## Run the test suite (nextest if available, else cargo test)
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		$(CARGO) nextest run --workspace $(CARGO_LOCKED); \
	else \
		$(CARGO) test --workspace $(CARGO_LOCKED); \
	fi

.PHONY: test-junit
test-junit: ## Run tests with the CI profile (emits JUnit report)
	$(CARGO) nextest run --workspace --profile ci $(CARGO_LOCKED)

.PHONY: doctest
doctest: ## Run documentation tests
	$(CARGO) test --workspace --doc $(CARGO_LOCKED)

# ---------------------------------------------------------------------------
##@ CI
# ---------------------------------------------------------------------------

.PHONY: ci
ci: quality security sqlx-verify test doctest ## Run the full pipeline locally (all gates)
	@echo "All local CI gates passed."

# Contributors are not required to install sqlx-cli; CI always runs this gate.
.PHONY: sqlx-verify
sqlx-verify: ## Verify .sqlx if sqlx-cli is installed, otherwise say so
	@if $(CARGO) sqlx --version >/dev/null 2>&1; then \
		$(MAKE) sqlx-check; \
	else \
		echo "sqlx-cli not installed - skipping the .sqlx check (CI runs it)"; \
	fi

# ---------------------------------------------------------------------------
##@ Build
# ---------------------------------------------------------------------------

.PHONY: build
build: ## Debug build of the whole workspace
	$(CARGO) build --workspace $(CARGO_LOCKED)

.PHONY: release
release: ## Optimized release build
	$(CARGO) build --release --workspace $(CARGO_LOCKED)

# ---------------------------------------------------------------------------
##@ Database schema
# ---------------------------------------------------------------------------
#
# Queries are verified at compile time against the metadata in .sqlx, which is
# committed. That is what lets every CI job except the test job build without a
# database - and what makes a query change visible in review.

MIGRATIONS ?= crates/store/migrations

.PHONY: migrate
migrate: ## Apply migrations to $DATABASE_URL
	$(CARGO) sqlx migrate run --source $(MIGRATIONS)

.PHONY: sqlx-prepare
sqlx-prepare: ## Regenerate .sqlx from the queries in the code (needs a live database)
	$(CARGO) sqlx prepare --workspace -- --all-targets

.PHONY: sqlx-check
sqlx-check: ## Verify .sqlx still matches the queries (CI: Tests)
	SQLX_OFFLINE=false $(CARGO) sqlx prepare --check --workspace -- --all-targets

# ---------------------------------------------------------------------------
##@ Local environment
# ---------------------------------------------------------------------------
#
# Any Compose-spec runtime works; override with COMPOSE="podman compose" if you
# do not run Docker.

COMPOSE ?= docker compose

.PHONY: db-up
db-up: ## Start PostgreSQL and wait until it accepts connections
	$(COMPOSE) up -d --wait --quiet-pull

.PHONY: db-down
db-down: ## Stop PostgreSQL and drop its volume
	$(COMPOSE) down -v

.PHONY: db-logs
db-logs: ## Follow the PostgreSQL log
	$(COMPOSE) logs -f postgres

.PHONY: db-shell
db-shell: ## Open psql inside the container
	$(COMPOSE) exec postgres psql -U $${POSTGRES_USER:-hydrant} -d $${POSTGRES_DB:-hydrant}

# ---------------------------------------------------------------------------
##@ Housekeeping
# ---------------------------------------------------------------------------

.PHONY: clean
clean: ## Remove build artifacts
	$(CARGO) clean
