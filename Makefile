# plurx developer tasks. `make` or `make help` lists everything.
# CI runs the same targets a developer does, so "green locally" means
# "green in CI" — there is no second, hidden set of commands.

CARGO ?= cargo
ANDROID_IMAGE ?= plurx-android-build
ANDROID_DATA_DIR ?=

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

# What a build from this tree stamps into the binary. Keep in step with
# crates/plurxd/build.rs — see docs/RELEASING.md.
VERSION := $(shell sed -n '/^\[workspace.package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
BUILD_REF := $(shell git describe --tags --always --dirty 2>/dev/null || echo unknown)

.PHONY: version
version: ## Print the version and git build stamp a build would report
	@echo "$(VERSION) ($(BUILD_REF))"

.PHONY: docker
docker: ## Build the container image
	docker build --build-arg PLURX_BUILD_REF="$(BUILD_REF)" -t plurx/plurxd:latest .

# The Compose deploy, as one command that cannot forget the stamp.
#
# `docker compose up -d --build` is what every doc told people to run, and it
# leaves PLURX_BUILD_REF empty — `.git` is outside the build context, so the
# image comes out stamped "unknown" and the running server cannot say which
# commit it is. That was "fixed" once, by teaching compose to forward the
# variable and documenting that deploys must set it. That is not a fix: it
# moves the work onto a human remembering an environment variable every single
# time, and the failure is silent. Nobody remembered, and the System page read
# "(unstamped build)" for weeks of daily deploys.
#
# So make the correct thing the easy thing. Named `docker-up` and not `deploy`
# because deploying plurx is not one thing: bare metal and systemd are equally
# supported (see deploy/README.md), and a target called `deploy` would claim to
# be the way to ship it while quietly meaning only one of the three. Container
# hosts get this; the other two stamp themselves from their own checkout,
# because `.git` is right there.
#
# **`cd deploy` — never `-f deploy/docker-compose.yml` from here.** Passing
# `-f` turns OFF Compose's automatic override discovery, so
# `docker-compose.override.yml` is silently ignored — and that file is where
# every host keeps the things this tracked repo cannot know: the media mounts,
# the GPU device passthrough, the ports. The stack still comes up, which is the
# worst part: it comes up with no media visible ("path does not resolve" on
# every library), no /dev/dri, and the transcoder fallen back to software x264.
# `-f` also moves the project directory to the repo root, so `deploy/.env` stops
# being read on the way past.
#
# The recipe below is character-for-character what somebody runs by hand in
# `deploy/`, with only the build arg added. That is the point: a convenience
# target that is not equivalent to the command it replaces is a trap, and this
# one sprang on the first real deploy.
.PHONY: docker-up
docker-up: ## Build + (re)start the Compose stack, stamping this commit into the image
	cd deploy && PLURX_BUILD_REF="$(BUILD_REF)" docker compose up -d --build
	@echo "up: $(VERSION) ($(BUILD_REF))"

.PHONY: release-check
release-check: ## Verify the tree is ready to tag the current version
	@test -z "$$(git status --porcelain)" || { echo "working tree is dirty — commit first"; exit 1; }
	@git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null \
	  && { echo "tag v$(VERSION) already exists — bump the version in Cargo.toml"; exit 1; } || true
	@grep -q '^## \[$(VERSION)\]' CHANGELOG.md \
	  || { echo "CHANGELOG.md has no '## [$(VERSION)]' section"; exit 1; }
	@$(MAKE) --no-print-directory check
	@echo "Ready: git tag -a v$(VERSION) -m 'v$(VERSION)' && git push && git push --tags"

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

.PHONY: android-publish
android-publish: android ## Build the APK + serve it from the web UI (ANDROID_DATA_DIR=/path/to/data)
	@test -n "$(ANDROID_DATA_DIR)" || { echo "set ANDROID_DATA_DIR to the server's data_dir, e.g. make android-publish ANDROID_DATA_DIR=~/.local/share/plurx"; exit 1; }
	cp clients/android/app/build/outputs/apk/debug/app-debug.apk "$(ANDROID_DATA_DIR)/plurx-android.apk"
	@echo "Published -> $(ANDROID_DATA_DIR)/plurx-android.apk (served at /download/plurx-android.apk, no restart needed)"

.PHONY: clean
clean: ## Remove build artifacts and coverage output
	$(CARGO) clean
	@rm -f lcov.info
