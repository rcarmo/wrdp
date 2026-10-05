# WRDP development and validation targets.
#
# Common flow:
#   make bootstrap          # install host packages + Rust toolchain
#   make check              # Rust type-check
#   make build              # debug build
#   make compositor-build   # build vendored compositor

SHELL := /usr/bin/env bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

RUST_VERSION ?= 1.94.0
FEATURES ?= default,vaapi
# Snapshot before redirecting TMPDIR; inherited snapshots survive recursive Make.
ifeq ($(origin PROJECT_ORIGINAL_TMPDIR),undefined)
PROJECT_ORIGINAL_TMPDIR := $(TMPDIR)
endif
# The repository-owned resolver validates explicit base/root overrides, selects
# CI runner/original/system temp or local workspace/system temp, then appends wrdp.
shell-quote = '$(subst ','"'"',$(1))'
WRDP_RESOLVER_VARS := PROJECT_TMP_BASE PROJECT_TMP_ROOT PROJECT_ORIGINAL_TMPDIR RUNNER_TEMP CI GITHUB_ACTIONS GITLAB_CI TF_BUILD CIRCLECI
WRDP_RESOLVER_ENV := $(foreach name,$(WRDP_RESOLVER_VARS),$(if $(filter undefined,$(origin $(name))),,$(name)=$(call shell-quote,$($(name)))))
WRDP_RESOLVED_TMP_ROOT := $(shell $(WRDP_RESOLVER_ENV) bash -c 'source scripts/project-tmp.sh; project_tmp_resolve wrdp')
ifeq ($(strip $(WRDP_RESOLVED_TMP_ROOT)),)
$(error Cannot resolve WRDP scratch root; fix PROJECT_TMP_BASE/PROJECT_TMP_ROOT or directory permissions)
endif
override PROJECT_TMP_ROOT := $(WRDP_RESOLVED_TMP_ROOT)
override WRDP_TMP_ROOT := $(PROJECT_TMP_ROOT)
# Export a supplied base, but do not invent an empty override for child tools.
ifneq ($(origin PROJECT_TMP_BASE),undefined)
export PROJECT_TMP_BASE
endif
export PROJECT_ORIGINAL_TMPDIR
TEST_ROOT := $(PROJECT_TMP_ROOT)/tests
LOG_ROOT := $(PROJECT_TMP_ROOT)/logs
export TEST_ROOT LOG_ROOT
WRDP_BUILD_ID ?= $(notdir $(CURDIR))
WRDP_RUN_ID := $(or $(WRDP_RUN_ID),$(shell date -u +%Y%m%dT%H%M%S)-$(shell echo $$$$))
export WRDP_RUN_ID
WRDP_RUN_DIR ?= $(WRDP_TMP_ROOT)/runs/make/$(WRDP_RUN_ID)
WRDP_TMPDIR ?= $(WRDP_RUN_DIR)/tmp
CARGO_TARGET_DIR ?= $(WRDP_TMP_ROOT)/build/$(WRDP_BUILD_ID)/cargo-target
COMPOSITOR_BUILD_DIR ?= $(WRDP_TMP_ROOT)/build/$(WRDP_BUILD_ID)/compositor
GROUND_INIT_BUILD_ROOT ?= $(WRDP_TMP_ROOT)/build/$(WRDP_BUILD_ID)/ground-init
WRDP_EVIDENCE_DIR ?= $(CURDIR)/.local/evidence
ASYNC_DIR ?= $(WRDP_TMP_ROOT)/runs/async/$(WRDP_BUILD_ID)
ASYNC_TARGET ?= check
ASYNC_LOG ?= $(WRDP_EVIDENCE_DIR)/async/$(ASYNC_TARGET)-$(WRDP_RUN_ID).log
ASYNC_PID ?= $(ASYNC_DIR)/$(ASYNC_TARGET).pid
ASYNC_STATUS ?= $(ASYNC_DIR)/$(ASYNC_TARGET).status
WRDP_COMPOSITOR_PREFIX ?= /usr
WRDP_COMPOSITOR_BUILDTYPE ?= release
WRDP_COMPOSITOR_DEFAULT_LIBRARY ?= static

RUSTUP_HOME_DIR ?= $(or $(firstword $(wildcard $(HOME)/.rustup)),$(HOME)/.rustup)
CARGO_HOME_DIR ?= $(WRDP_TMP_ROOT)/cache/cargo
# rustup is an installed toolchain, not a disposable Cargo download cache.
RUSTUP_BIN ?= $(or $(shell command -v rustup 2>/dev/null),$(HOME)/.cargo/bin/rustup)
RUSTUP_INSTALL_CARGO_HOME ?= $(patsubst %/bin/,%,$(dir $(RUSTUP_BIN)))
RUSTUP_ENV := RUSTUP_HOME='$(RUSTUP_HOME_DIR)' CARGO_HOME='$(CARGO_HOME_DIR)'
CARGO ?= $(RUSTUP_ENV) $(RUSTUP_BIN) run $(RUST_VERSION) cargo
MESON ?= meson
DOCKER ?= docker
APT_GET ?= apt-get
SUDO ?= $(shell if [ "$$(id -u)" = 0 ]; then printf ""; else printf "sudo"; fi)
PYTHON := /usr/bin/python3
WRDP_SOURCE_DIR := $(abspath .)
PROVISION_USER ?=
export WRDP_SOURCE_DIR PROVISION_USER

TMPDIR := $(WRDP_TMPDIR)
TMP := $(TMPDIR)
TEMP := $(TMPDIR)
CARGO_HOME := $(CARGO_HOME_DIR)
XDG_CACHE_HOME := $(WRDP_TMP_ROOT)/cache/xdg
PYTHONPYCACHEPREFIX := $(WRDP_TMP_ROOT)/cache/python/pycache
PIP_CACHE_DIR := $(WRDP_TMP_ROOT)/cache/python/pip
CCACHE_DIR := $(WRDP_TMP_ROOT)/cache/ccache
SCCACHE_DIR := $(WRDP_TMP_ROOT)/cache/sccache
BUN_INSTALL_CACHE_DIR := $(WRDP_TMP_ROOT)/cache/bun
npm_config_cache := $(WRDP_TMP_ROOT)/cache/npm
export PROJECT_TMP_ROOT WRDP_TMP_ROOT WRDP_BUILD_ID WRDP_RUN_ID WRDP_RUN_DIR WRDP_TMPDIR
export TMPDIR TMP TEMP CARGO_HOME CARGO_TARGET_DIR COMPOSITOR_BUILD_DIR
export XDG_CACHE_HOME PYTHONPYCACHEPREFIX PIP_CACHE_DIR CCACHE_DIR SCCACHE_DIR
export BUN_INSTALL_CACHE_DIR npm_config_cache GROUND_INIT_BUILD_ROOT WRDP_EVIDENCE_DIR ASYNC_DIR
CARGO_ENV := TMPDIR='$(WRDP_TMPDIR)' CARGO_HOME='$(CARGO_HOME_DIR)' CARGO_TARGET_DIR='$(CARGO_TARGET_DIR)'
CARGO_COMMON := --features $(FEATURES)

.PHONY: prepare-paths paths
prepare-paths:
	@$(PYTHON) -B scripts/dev-paths.py prepare

paths: ## Print resolved development paths without building or creating directories.
	@$(PYTHON) -B scripts/dev-paths.py show

# Order-only prerequisite also covers recursive Make invocations and subprocesses.
deps rustup-install rustup toolchain-check metadata fetch fmt fmt-fix check clippy test test-paths build build-release compositor-configure compositor-build provision-system provision-user validate-production async-start: | prepare-paths

.PHONY: test-paths
test-paths: ## Run focused path/provisioning tests with retained Python CPU/heap profiles.
	$(PYTHON) scripts/profile-path-tests.py

# Host packages needed for local Rust builds, compositor builds,
# and day-to-day repository work on Debian.
APT_PACKAGES := \
	build-essential \
	ca-certificates \
	clang \
	cmake \
	curl \
	debhelper \
	devscripts \
	dpkg-dev \
	gettext \
	git \
	jq \
	libcairo2-dev \
	libdbus-1-dev \
	libdrm-dev \
	libfuse3-dev \
	libgbm-dev \
	libglib2.0-dev \
	libinput-dev \
	libpam0g-dev \
	libpango1.0-dev \
	libpipewire-0.3-dev \
	libpixman-1-dev \
	libpng-dev \
	librsvg2-dev \
	libsfdo-dev \
	libspa-0.2-dev \
	libssl-dev \
	libva-dev \
	libwayland-dev \
	libwlroots-0.18-dev \
	libxml2-dev \
	libxcb-icccm4-dev \
	libxcb1-dev \
	libxkbcommon-dev \
	meson \
	nasm \
	wayland-protocols \
	ninja-build \
	pkgconf \
	ripgrep

.PHONY: help
help: ## Show this help.
	@awk 'BEGIN {FS = ":.*##"; printf "\nwrdp development targets\n\n"} /^[a-zA-Z0-9_.-]+:.*##/ {printf "  %-22s %s\n", $$1, $$2} END {printf "\nVariables: RUST_VERSION=%s FEATURES=%s WRDP_TMPDIR=%s CARGO_TARGET_DIR=%s\n", "$(RUST_VERSION)", "$(FEATURES)", "$(WRDP_TMPDIR)", "$(CARGO_TARGET_DIR)"}' $(MAKEFILE_LIST)

.PHONY: bootstrap
bootstrap: deps rustup ## Install host dependencies and Rust toolchain.

.PHONY: deps
deps: ## Install Debian packages needed for development and packaging.
	$(SUDO) env TMPDIR='$(TMPDIR)' TMP='$(TMP)' TEMP='$(TEMP)' $(APT_GET) -o Dir::Cache='$(WRDP_TMP_ROOT)/cache/apt' update
	$(SUDO) env TMPDIR='$(TMPDIR)' TMP='$(TMP)' TEMP='$(TEMP)' $(APT_GET) -o Dir::Cache='$(WRDP_TMP_ROOT)/cache/apt' install -y $(APT_PACKAGES)

.PHONY: rustup-install
rustup-install: ## Install rustup if it is not already available.
	@if [ -x '$(RUSTUP_BIN)' ]; then \
		echo "rustup already installed: $$($(RUSTUP_ENV) $(RUSTUP_BIN) --version)"; \
	else \
		curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | RUSTUP_HOME='$(RUSTUP_HOME_DIR)' CARGO_HOME='$(RUSTUP_INSTALL_CARGO_HOME)' sh -s -- -y --profile minimal --default-toolchain none; \
		echo 'Installed toolchain proxies under $(RUSTUP_INSTALL_CARGO_HOME)/bin; Cargo downloads stay project-scoped.'; \
	fi

.PHONY: rustup
rustup: rustup-install ## Install/update the pinned Rust toolchain and standard components.
	$(RUSTUP_ENV) $(RUSTUP_BIN) toolchain install $(RUST_VERSION) --profile minimal --component rustfmt --component clippy
	$(RUSTUP_ENV) $(RUSTUP_BIN) default $(RUST_VERSION)

.PHONY: toolchain-check
toolchain-check: ## Print the active pinned Rust toolchain versions.
	$(RUSTUP_ENV) $(RUSTUP_BIN) run $(RUST_VERSION) rustc --version
	$(CARGO) --version
	$(RUSTUP_ENV) $(RUSTUP_BIN) run $(RUST_VERSION) rustfmt --version || true
	$(RUSTUP_ENV) $(RUSTUP_BIN) run $(RUST_VERSION) cargo clippy --version || true

.PHONY: metadata
metadata: ## Validate Cargo metadata and print package name/version/license.
	$(CARGO_ENV) $(CARGO) metadata --format-version 1 --no-deps | jq -r '.packages[] | select(.name=="wrdp") | [.name,.version,.license] | @tsv'

.PHONY: fetch
fetch: ## Fetch Rust dependencies for the pinned toolchain.
	$(CARGO_ENV) $(CARGO) fetch

.PHONY: fmt
fmt: ## Check Rust formatting.
	$(CARGO_ENV) $(CARGO) fmt --all -- --check

.PHONY: fmt-fix
fmt-fix: ## Apply Rust formatting.
	$(CARGO_ENV) $(CARGO) fmt --all

.PHONY: check
check: ## Type-check library and binaries.
	$(CARGO_ENV) $(CARGO) check --lib --bins $(CARGO_COMMON)

.PHONY: clippy
clippy: ## Run clippy with the selected feature set.
	$(CARGO_ENV) $(CARGO) clippy --lib --bins $(CARGO_COMMON)

.PHONY: test
test: ## Run Rust unit tests.
	$(CARGO_ENV) $(CARGO) test --lib --bins $(CARGO_COMMON)

.PHONY: build
build: ## Build debug binaries.
	$(CARGO_ENV) $(CARGO) build --bins $(CARGO_COMMON)

.PHONY: build-release
build-release: ## Build optimized release binaries.
	$(CARGO_ENV) $(CARGO) build --release --bins $(CARGO_COMMON)

.PHONY: smoke-help
smoke-help: ## Smoke-test the already-built wrdp binary CLI. Run async-build first.
	test -x $(CARGO_TARGET_DIR)/debug/wrdp
	$(CARGO_TARGET_DIR)/debug/wrdp --help >/dev/null

.PHONY: smoke-systemd
smoke-systemd: ## Smoke-test the installed systemd socket/service state without changing source.
	systemctl is-active --quiet wrdp.socket
	ss -ltn | grep -q ':3389 '
	systemctl status wrdp.service --no-pager -l >/dev/null || true

.PHONY: compositor-configure
compositor-configure: ## Configure the vendored wrdp compositor build.
	$(MESON) setup '$(COMPOSITOR_BUILD_DIR)' vendor/wrdp-compositor \
		--prefix=$(WRDP_COMPOSITOR_PREFIX) \
		--buildtype=$(WRDP_COMPOSITOR_BUILDTYPE) \
		--default-library=$(WRDP_COMPOSITOR_DEFAULT_LIBRARY)

.PHONY: compositor-build
compositor-build: ## Build the vendored wrdp compositor.
	@if [ ! -f '$(COMPOSITOR_BUILD_DIR)/build.ninja' ]; then \
		$(MAKE) compositor-configure; \
	fi
	$(MESON) compile -C '$(COMPOSITOR_BUILD_DIR)'

.PHONY: compositor-clean
compositor-clean: ## Remove only this checkout's compositor build; refuse active async jobs.
	@$(PYTHON) -B scripts/dev-paths.py clean-compositor

.PHONY: install-session-defaults
install-session-defaults: ## Install the minimal managed desktop configuration and Platinum theme.
	$(SUDO) install -d -m 0755 /etc/wrdp/labwc /etc/wrdp/waybar /etc/wrdp/mako /etc/wrdp/wallpaper /usr/lib/wrdp /usr/share/themes
	$(SUDO) rm -rf /usr/share/themes/PlatinumTheme-wrdp-compositor
	$(SUDO) cp -a vendor/wrdp-compositor/themes/PlatinumTheme-wrdp-compositor /usr/share/themes/
	$(SUDO) chmod -R a+rX /usr/share/themes/PlatinumTheme-wrdp-compositor
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/labwc/autostart /etc/wrdp/labwc/autostart
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/labwc/menu.xml /etc/wrdp/labwc/menu.xml
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/labwc/rc.xml /etc/wrdp/labwc/rc.xml
	$(SUDO) install -m 0755 vendor/wrdp-compositor/contrib/wrdp/labwc/shutdown /etc/wrdp/labwc/shutdown
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/waybar/config.jsonc /etc/wrdp/waybar/config.jsonc
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/waybar/style.css /etc/wrdp/waybar/style.css
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/mako/config /etc/wrdp/mako/config
	$(SUDO) install -m 0644 vendor/wrdp-compositor/contrib/wrdp/wallpaper/wallpaper.conf /etc/wrdp/wallpaper/wallpaper.conf
	$(SUDO) install -m 0755 vendor/wrdp-compositor/contrib/wrdp/bin/wrdp-desktop-action /usr/lib/wrdp/wrdp-desktop-action
	$(SUDO) install -m 0755 vendor/wrdp-compositor/contrib/wrdp/bin/wrdp-desktop-session /usr/lib/wrdp/wrdp-desktop-session
	$(SUDO) install -m 0755 vendor/wrdp-compositor/contrib/wrdp/bin/wrdp-wallpaper /usr/lib/wrdp/wrdp-wallpaper

.PHONY: provision-system
provision-system: ## Provision packages and system-owned desktop files (run as root).
	@test "$$(id -u)" -eq 0 || { echo "provision-system must run as root" >&2; exit 1; }
	@$(PYTHON) -c 'import yaml' >/dev/null 2>&1 || { apt-get -o Dir::Cache='$(WRDP_TMP_ROOT)/cache/apt' update && apt-get -o Dir::Cache='$(WRDP_TMP_ROOT)/cache/apt' install -y python3-yaml; }
	$(PYTHON) vendor/ground-init/ground-init.py ground-init.system.yaml

.PHONY: provision-user
provision-user: ## Provision one user's preferences (PROVISION_USER=name; run as root).
	@test "$$(id -u)" -eq 0 || { echo "provision-user must run as root" >&2; exit 1; }
	@$(PYTHON) scripts/provision-user.py

.PHONY: validate-production
validate-production: metadata fmt ## Run release-oriented Rust and compositor gates.
	$(CARGO_ENV) $(CARGO) check --locked --lib --bins --all-features
	$(CARGO_ENV) $(CARGO) test --locked --lib --bins --all-features
	$(CARGO_ENV) $(CARGO) check --locked --lib --bins --no-default-features
	$(CARGO_ENV) $(CARGO) test --locked --lib --bins --no-default-features
	$(CARGO_ENV) $(CARGO) build --locked --release --bins $(CARGO_COMMON)
	$(MAKE) compositor-build

.PHONY: lifecycle
lifecycle: metadata check build compositor-build ## Run the standard local development lifecycle.

.PHONY: ci
ci: metadata fmt check test build compositor-build ## Run CI-style checks.

.PHONY: async-start
async-start: ## Start ASYNC_TARGET in the background; writes log/pid/status under ASYNC_DIR.
	@case '$(firstword $(MAKEFLAGS))' in --*) ;; *n*) echo 'dry run: would start $(ASYNC_TARGET)'; exit 0;; esac; \
	mkdir -p '$(ASYNC_DIR)' "$(dir $(ASYNC_LOG))"; \
	if [ -f '$(ASYNC_PID)' ] && kill -0 "$$(cat '$(ASYNC_PID)')" 2>/dev/null; then \
		echo "$(ASYNC_TARGET) already running: pid $$(cat '$(ASYNC_PID)')"; exit 0; \
	fi; \
	printf '%s\n' '$(ASYNC_LOG)' > '$(ASYNC_DIR)/$(ASYNC_TARGET).log-path'; \
	printf 'running\n' > '$(ASYNC_STATUS)'; \
	nohup bash -c 'cd "$(CURDIR)" && export HOME="$(HOME)" WRDP_TMPDIR="$(WRDP_TMPDIR)" CARGO_TARGET_DIR="$(CARGO_TARGET_DIR)" FEATURES="$(FEATURES)" RUST_VERSION="$(RUST_VERSION)"; echo "[$$(date -Is)] START make $(ASYNC_TARGET)"; ionice -c2 -n7 nice -n 15 $(MAKE) --no-print-directory $(ASYNC_TARGET); rc=$$?; echo "[$$(date -Is)] END make $(ASYNC_TARGET) status=$$rc"; echo $$rc > "$(ASYNC_STATUS)"; exit $$rc' > '$(ASYNC_LOG)' 2>&1 & echo $$! > '$(ASYNC_PID)'; \
	echo "started $(ASYNC_TARGET): pid $$(cat '$(ASYNC_PID)') log=$(ASYNC_LOG) status=$(ASYNC_STATUS)"

.PHONY: async-check async-test async-build async-ci async-h264-check
async-check: ## Start `make check` asynchronously.
	@$(MAKE) --no-print-directory async-start ASYNC_TARGET=check
async-test: ## Start `make test` asynchronously.
	@$(MAKE) --no-print-directory async-start ASYNC_TARGET=test
async-build: ## Start `make build` asynchronously.
	@$(MAKE) --no-print-directory async-start ASYNC_TARGET=build
async-ci: ## Start `make ci` asynchronously.
	@$(MAKE) --no-print-directory async-start ASYNC_TARGET=ci
async-h264-check: ## Start a viable H.264-only check asynchronously.
	@$(MAKE) --no-print-directory async-start ASYNC_TARGET=check FEATURES='h264 wayland portal-generic'

.PHONY: async-status
async-status: ## Show async job status and the tail of its log.
	@if [ -f '$(ASYNC_PID)' ] && kill -0 "$$(cat '$(ASYNC_PID)')" 2>/dev/null; then \
		echo "$(ASYNC_TARGET): running pid=$$(cat '$(ASYNC_PID)')"; \
	else \
		echo "$(ASYNC_TARGET): not running status=$$(cat '$(ASYNC_STATUS)' 2>/dev/null || echo unknown)"; \
	fi
	@log=$$(cat '$(ASYNC_DIR)/$(ASYNC_TARGET).log-path' 2>/dev/null || true); if [ -f "$$log" ]; then tail -n 80 "$$log"; else echo 'no saved log'; fi

.PHONY: async-log
async-log: ## Tail the async job log for ASYNC_TARGET.
	@log=$$(cat '$(ASYNC_DIR)/$(ASYNC_TARGET).log-path' 2>/dev/null || true); tail -n 160 "$$log" 2>/dev/null || echo 'no saved log'

.PHONY: async-stop
async-stop: ## Stop the async job for ASYNC_TARGET if it is still running.
	@if [ -f '$(ASYNC_PID)' ] && kill -0 "$$(cat '$(ASYNC_PID)')" 2>/dev/null; then \
		pid="$$(cat '$(ASYNC_PID)')"; kill "$$pid"; echo stopped "$$pid"; \
	else \
		echo "$(ASYNC_TARGET): no running pid"; \
	fi

.PHONY: clean
clean: ## Remove only this checkout's build output; preserve caches, toolchains and evidence.
	@$(PYTHON) -B scripts/dev-paths.py clean-build

.PHONY: async-clean
async-clean: ## Remove inactive async scratch and build output; preserve retained logs.
	@$(PYTHON) -B scripts/dev-paths.py clean-async

.PHONY: distclean
distclean: clean async-clean ## Clean owned build/scratch only; never delete source, caches or evidence.
