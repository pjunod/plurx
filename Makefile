# plurx developer tasks. `make` or `make help` lists everything.
# CI enters through `scripts/validate`, which composes the same Make targets a
# developer runs. `make validate` is the ordinary local entry point; `make
# check` remains the portable baseline inside it.

CARGO ?= cargo
ANDROID_IMAGE ?= plurx-android-build
ANDROID_PLATFORM ?= linux/amd64
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
run: ## Run the server (http://localhost:32400)
	$(CARGO) run -p plurxd

.PHONY: fmt
fmt: ## Auto-format all code
	$(CARGO) fmt --all

.PHONY: test
test: ## Run the test suite
	$(CARGO) test --workspace

## ---- baseline gates ----------------------------------------------------

.PHONY: fmt-check
fmt-check: ## Verify formatting without changing files
	$(CARGO) fmt --all --check

.PHONY: lint
lint: ## Clippy across the workspace, warnings are errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: rust-check
rust-check: fmt-check lint test ## Rust format, lint, and workspace tests

.PHONY: history-check
history-check: ## Verify every corrective commit has current regression evidence
	@scripts/history-audit --report target/validation/history.json

.PHONY: operations-check
operations-check: ## Verify deploy, CI, container, and client shipping contracts
	@python3 -m unittest discover -s tests/operations -p 'test_*.py'

.PHONY: check
check: validation-lint history-check operations-check rust-check ## History + operations + catalog + Rust baseline

.PHONY: hiqlite-spike
hiqlite-spike: ## Run the isolated M0 raft/SQLite semantic proof
	$(CARGO) test --manifest-path spikes/hiqlite-m0/Cargo.toml \
	  --test hiqlite_m0 -- --nocapture

.PHONY: hiqlite-baseline
hiqlite-baseline: ## Measure the manual M0 one-voter cost gate on a quiet host
	$(CARGO) test --release --manifest-path spikes/hiqlite-m0/Cargo.toml \
	  --test hiqlite_m0 \
	  single_voter_cost_stays_inside_the_m0_budget -- --ignored --exact --nocapture

## ---- functionality-point validation -----------------------------------

# `make check` remains the mandatory baseline. The validator sits above it:
# the catalog maps changed paths to named behavior contracts and adds the
# surface-specific checks that ordinary Rust compilation cannot see.
.PHONY: validate-help
validate-help: ## Explain the validation workflow and UI golden commands
	@printf '%s\n' \
	  'Functionality-point validation maps changed files to user-visible promises.' \
	  '' \
	  '  make validate-plan    Explain what the staged change selects; run nothing' \
	  '  make validate-staged  Validate the staged change (the normal local loop)' \
	  '  make validate         Validate every point with the commit profile' \
	  '  make validate-full    Add browser, client, and packaging checks' \
	  '  make validate-nightly Exhaustive playback, recovery, bounds, and packaging' \
	  '  make validation-lint  Check the catalog and path ownership only' \
	  '  make history-check    Map every corrective commit to current evidence' \
	  '  make operations-check Pin deploy, CI, container, and ship contracts' \
	  '' \
	  'UI structure uses a reviewed answer key, tests/ui-structure.golden:' \
	  '  make ui-check         Compare structure and enforce accessibility invariants' \
	  '  make ui-golden        Rewrite the answer after an intentional UI change' \
	  '' \
	  'Details: docs/VALIDATION.md'

.PHONY: validation-lint
validation-lint: ## Verify every governed file maps to a valid functionality point
	@scripts/validate lint

.PHONY: validate-plan
validate-plan: ## Explain which points and checks the staged diff selects
	@scripts/validate plan --profile commit --staged

.PHONY: validate
validate: ## Run the commit-profile checks for every functionality point
	@scripts/validate run --profile commit --all

.PHONY: validate-staged
validate-staged: ## Run the commit-profile checks selected by the staged diff
	@scripts/validate run --profile commit --staged

.PHONY: validate-full
validate-full: ## Run extended browser, client, and packaging validations
	@scripts/validate run --profile full --all

.PHONY: validate-nightly
validate-nightly: ## Run exhaustive playback, recovery, bounds, clients, UI, and packaging
	@scripts/validate run --profile nightly --all --strict

.PHONY: coverage
coverage: ## Line coverage (installs cargo-llvm-cov on first run); writes lcov.info
	@$(CARGO) llvm-cov --version >/dev/null 2>&1 || $(CARGO) install cargo-llvm-cov
	$(CARGO) llvm-cov --workspace --lcov --output-path lcov.info
	@$(CARGO) llvm-cov --workspace --summary-only

## ---- playback lab ------------------------------------------------------

.PHONY: playback-doctor
playback-doctor: ## Check playback-lab codecs, filters, browser, and server binary
	@scripts/playback-lab doctor

.PHONY: playback-fixtures
playback-fixtures: ## Build + ffprobe the deterministic playback corpus
	@scripts/playback-lab fixtures

.PHONY: playback-smoke
playback-smoke: ## Run the risk-weighted end-to-end playback matrix in Chrome
	@scripts/playback-lab run --suite smoke

.PHONY: playback-smoke-safari
playback-smoke-safari: ## Run the playback smoke matrix in Safari (macOS)
	@scripts/playback-lab run --suite smoke --browser safari

.PHONY: playback-smoke-edge
playback-smoke-edge: ## Run the playback smoke matrix in Microsoft Edge
	@scripts/playback-lab run --suite smoke --browser edge

.PHONY: playback-smoke-firefox
playback-smoke-firefox: ## Run the playback smoke matrix in Firefox (needs geckodriver)
	@scripts/playback-lab run --suite smoke --browser firefox

.PHONY: playback-full
playback-full: ## Run every fixture x quality plus playback restart cases
	@scripts/playback-lab run --suite full

## ---- web UI baseline ---------------------------------------------------

# The layout work rearranges one 6000-line file with no component tests under
# it. `ui-baseline` captures the shipped UI — every registered layout, nine
# routes, two viewports — so a refactor can be *shown* to have changed nothing.
#
# Two tiers, and the difference is the whole design (see the script's header,
# and docs/UI-LAYOUTS-G3-DECISION.md §5/R1). The STRUCTURAL tier —
# tests/ui-structure.golden — is a reviewed answer key designed to be committed
# and enforced by `ui-check`. Nothing in it is a pixel, a path or a clock, so
# it is the same file on every machine. The PIXEL tier stays in
# target/: a PNG hash depends on the Chromium build and on where the fixture
# library sits on disk, so committing it would commit a fact about one laptop
# and go red on every other.
.PHONY: ui-baseline
ui-baseline: ## Capture the UI baseline for every layout (both tiers, into target/)
	@scripts/ui-baseline --self-host

# The gate. Fails on any structural drift and prints which layout, which route,
# which viewport and which key moved. It also rejects deterministic a11y defects
# (unnamed controls, broken ARIA references, duplicate ids, and missing alt).
.PHONY: ui-check
ui-check: ## Sweep every layout and fail if the structural golden moved
	@scripts/ui-baseline --self-host --check

# Regenerating the golden is a deliberate act that shows up in a git diff, never
# a side effect of a normal run — a golden that rewrites itself asserts nothing.
# Run this when you MEANT to change the UI, then read the diff before committing.
.PHONY: ui-golden
ui-golden: ## Rewrite tests/ui-structure.golden after an intended UI change
	@scripts/ui-baseline --self-host --update

# `make check` cannot see either of these. index.html is include_str!-embedded,
# so a JS syntax error in it compiles, links, passes every Rust test, and then
# serves a blank page; and the theme tables are data, so a token pair that
# fails contrast is not a type error anywhere. Run this on any web change.
.PHONY: web-check
web-check: ## Test playback policy, embedded JS, and every shipped theme
	@node tests/playback/web-policy.test.js
	@scripts/js-check
	@scripts/contrast-check --from-index crates/plurxd/src/web/index.html \
		--foregrounds='--text,--muted,--prose,--accent,--good,--warn,--bad' \
		--allow scripts/contrast-allow.txt

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

.PHONY: container-smoke
container-smoke: docker ## Build, start, probe, restart, and re-probe the container
	@scripts/container-smoke plurx/plurxd:latest

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
	@scripts/validate run --profile ci --all --strict
	@echo "Ready: git tag -a v$(VERSION) -m 'v$(VERSION)' && git push && git push --tags"

.PHONY: hooks
hooks: ## Install the functionality-point pre-commit validator
	@mkdir -p .git/hooks
	@install -m 0755 scripts/pre-commit .git/hooks/pre-commit
	@echo "Installed .git/hooks/pre-commit — it runs make validate-staged."
	@echo "Bypass one run with 'git commit --no-verify'."

## ---- apple clients -----------------------------------------------------

.PHONY: apple-test
apple-test: ## Generate the Xcode project and test the shared suite on iOS + tvOS
	cd clients/apple && xcodegen generate
	cd clients/apple && xcodebuild -project plurx.xcodeproj -scheme plurx-iOS \
	  -destination "$${APPLE_IOS_SIM:-platform=iOS Simulator,name=iPhone 17 Pro}" \
	  CODE_SIGNING_ALLOWED=NO test
	cd clients/apple && xcodebuild -project plurx.xcodeproj -scheme plurx-tvOS \
	  -destination "$${APPLE_TVOS_SIM:-platform=tvOS Simulator,name=Apple TV 4K (3rd generation)}" \
	  CODE_SIGNING_ALLOWED=NO test

## ---- android client ----------------------------------------------------

.PHONY: android-image
android-image: ## Build the pinned Android build-env image (JDK 25 + SDK)
	docker build --platform $(ANDROID_PLATFORM) -t $(ANDROID_IMAGE) clients/android

.PHONY: android-test
android-test: android-image ## Run Android JVM unit tests + lint in the pinned image
	docker run --rm --platform $(ANDROID_PLATFORM) \
	  -u $$(id -u):$$(id -g) -e HOME=/tmp \
	  -e GRADLE_USER_HOME=/workspace/clients/android/.gradle-validation \
	  -v "$(CURDIR)":/workspace -w /workspace/clients/android \
	  $(ANDROID_IMAGE) ./gradlew --no-daemon testDebugUnitTest lintDebug

.PHONY: android-instrumentation-build
android-instrumentation-build: android-image ## Build app + test APKs for an emulator/device run
	docker run --rm --platform $(ANDROID_PLATFORM) \
	  -u $$(id -u):$$(id -g) -e HOME=/tmp \
	  -e GRADLE_USER_HOME=/workspace/clients/android/.gradle-validation \
	  -v "$(CURDIR)":/workspace -w /workspace/clients/android \
	  $(ANDROID_IMAGE) ./gradlew --no-daemon assembleDebug assembleDebugAndroidTest

.PHONY: android-instrumentation-run
android-instrumentation-run: ## Install and run instrumented tests (set PLURX_ANDROID_SERIAL)
	@test -n "$${PLURX_ANDROID_SERIAL:-}" || { echo "set PLURX_ANDROID_SERIAL to a disposable emulator/device serial"; exit 1; }
	adb -s "$${PLURX_ANDROID_SERIAL}" wait-for-device
	adb -s "$${PLURX_ANDROID_SERIAL}" install -r clients/android/app/build/outputs/apk/debug/app-debug.apk
	adb -s "$${PLURX_ANDROID_SERIAL}" install -r clients/android/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk
	adb -s "$${PLURX_ANDROID_SERIAL}" shell am instrument -w \
	  tv.plurx.app.test/androidx.test.runner.AndroidJUnitRunner

.PHONY: android-instrumentation
android-instrumentation: android-instrumentation-build android-instrumentation-run ## Run UI tests on an explicitly selected disposable device

.PHONY: android
android: android-image ## Build the Android debug APK in Docker (no host JDK/SDK)
	docker run --rm \
	  --platform $(ANDROID_PLATFORM) \
	  -u $$(id -u):$$(id -g) -e HOME=/tmp \
	  -e GRADLE_USER_HOME=/workspace/clients/android/.gradle-docker \
	  -v "$(CURDIR)":/workspace -w /workspace/clients/android \
	  $(ANDROID_IMAGE) ./gradlew --no-daemon :app:assembleDebug
	@echo "→ clients/android/app/build/outputs/apk/debug/app-debug.apk"

.PHONY: apk
apk: android ## Build the Android debug APK (alias for android)

.PHONY: android-publish
android-publish: android ## Build the APK + serve it from the web UI (ANDROID_DATA_DIR=/path/to/data)
	@test -n "$(ANDROID_DATA_DIR)" || { echo "set ANDROID_DATA_DIR to the server's data_dir, e.g. make android-publish ANDROID_DATA_DIR=~/.local/share/plurx"; exit 1; }
	cp clients/android/app/build/outputs/apk/debug/app-debug.apk "$(ANDROID_DATA_DIR)/plurx-android.apk"
	@echo "Published -> $(ANDROID_DATA_DIR)/plurx-android.apk (served at /download/plurx-android.apk, no restart needed)"

.PHONY: clean
clean: ## Remove build artifacts and coverage output
	$(CARGO) clean
	@rm -f lcov.info
