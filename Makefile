# plurx developer tasks. `make` or `make help` lists everything.
# CI runs the same targets a developer does, so "green locally" means
# "green in CI" — there is no second, hidden set of commands.

CARGO ?= cargo
ANDROID_IMAGE ?= plurx-android-build

.DEFAULT_GOAL := help

.PHONY: help
help: ## List available targets
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
	  | sort \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

## ---- day to day --------------------------------------------------------

.PHONY: build
build: ## Debug build of the whole workspace
	$(CARGO) build --workspace

.PHONY: run
run: ## Run the server (http://localhost:32600)
	$(CARGO) run -p plurxd

.PHONY: fmt
fmt: ## Auto-format all code
	$(CARGO) fmt --all

.PHONY: test
test: ## Run the test suite
	$(CARGO) test --workspace

## ---- gates (what CI enforces) -----------------------------------------

.PHONY: fmt-check
fmt-check: ## Verify formatting without changing files
	$(CARGO) fmt --all --check

.PHONY: lint
lint: ## Clippy across the workspace, warnings are errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: check
check: fmt-check lint test ## fmt-check + lint + test — the full gate CI runs

.PHONY: coverage
coverage: ## Line coverage (installs cargo-llvm-cov on first run); writes lcov.info
	@$(CARGO) llvm-cov --version >/dev/null 2>&1 || $(CARGO) install cargo-llvm-cov
	$(CARGO) llvm-cov --workspace --lcov --output-path lcov.info
	@$(CARGO) llvm-cov --workspace --summary-only

## ---- packaging & setup -------------------------------------------------

.PHONY: docker
docker: ## Build the container image
	docker build -t plurx/plurxd:latest .

.PHONY: hooks
hooks: ## Install the git pre-commit hook (runs `make check`)
	@mkdir -p .git/hooks
	@install -m 0755 scripts/pre-commit .git/hooks/pre-commit
	@echo "Installed .git/hooks/pre-commit — bypass a run with 'git commit --no-verify'."

## ---- android client ----------------------------------------------------

.PHONY: android-image
android-image: ## Build the pinned Android build-env image (JDK 17 + SDK)
	docker build -t $(ANDROID_IMAGE) clients/android

.PHONY: android
android: android-image ## Build the Android debug APK in Docker (no host JDK/SDK)
	docker run --rm \
	  -u $$(id -u):$$(id -g) -e HOME=/tmp \
	  -e GRADLE_USER_HOME=/workspace/clients/android/.gradle-docker \
	  -v "$(CURDIR)":/workspace -w /workspace/clients/android \
	  $(ANDROID_IMAGE) ./gradlew --no-daemon :app:assembleDebug
	@echo "→ clients/android/app/build/outputs/apk/debug/app-debug.apk"

.PHONY: clean
clean: ## Remove build artifacts and coverage output
	$(CARGO) clean
	@rm -f lcov.info
