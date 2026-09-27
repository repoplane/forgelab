SHELL := /bin/bash
ADMIN := labadmin
PASS  := labadmin-not-a-secret
URL   := http://localhost:3000

# A release passes VERSION=<tag>; a local build describes the checkout it came from.
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
export FORGELAB_VERSION := $(VERSION)

# Where the repoplane/fleets checkout is: the golden-lock and scale tests read it.
FORGELAB_FLEETS_DIR ?= $(abspath ../fleets)
export FORGELAB_FLEETS_DIR

# <rust target>/<name>. The name is what `uname -s`_`uname -m` prints on that platform, so
# the install one-liner in the README can build the asset URL with no script in between.
LINUX_TARGETS  := x86_64-unknown-linux-musl/Linux_x86_64 aarch64-unknown-linux-musl/Linux_aarch64
DARWIN_TARGETS := x86_64-apple-darwin/Darwin_x86_64 aarch64-apple-darwin/Darwin_arm64

.PHONY: help build lint unit test scale lock ci up down proxy dist dist-linux dist-darwin checksums

## Show this help
help:
	@awk '/^## /{doc=substr($$0,4); next} \
	      /^#/{next} \
	      /^[a-z][a-z-]*:/{if(doc!=""){printf "  \033[1m%-11s\033[0m %s\n", substr($$1,1,length($$1)-1), doc; doc=""}; next} \
	      {doc=""}' $(MAKEFILE_LIST)

## Build ./bin/forgelab
build:
	cargo build --release -p forgelab
	@mkdir -p bin && cp target/release/forgelab bin/forgelab

## Check formatting, clippy, and that the CLI links no test-only crate
lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	@if cargo tree -p forgelab -e normal --prefix none | grep -qE 'testcontainers|bollard|forgelab-faultproxy'; then \
		echo "the CLI links a test-only crate"; exit 1; fi

## Run the tests that need no Docker (unit, golden locks, CLI)
unit:
	cargo test --workspace

## Run every test, including the end-to-end suite against a throwaway Forgejo (needs Docker)
test: unit
	FORGELAB_E2E=1 cargo test -p forgelab --test e2e

## Run the 108-repository scale fleet through the fault layer (needs Docker and ../fleets)
scale:
	FORGELAB_E2E=1 FORGELAB_SCALE=1 cargo test -p forgelab --test e2e scale_faults -- --test-threads=1 --nocapture

## Regenerate examples/fleet/fleet.lock.json after editing the example fleet
lock:
	FORGELAB_UPDATE_LOCK=1 cargo test -p forgelab --test golden_lock examples_fleet

# The workflow calls the same targets, so a green `make ci` here means a green run there.
## Everything CI runs on a pull request: lint, then test
ci: lint test

# A token's secret is only revealed once, so a name left over from an earlier `make up`
# against the same instance is deleted before it is minted again.
## Boot a local Forgejo on :3000 and print a token for it
up:
	@docker compose up -d --wait --force-recreate --renew-anon-volumes
	@docker exec -u git forgelab-forgejo forgejo admin user create \
		--username $(ADMIN) --password $(PASS) --email admin@forgelab.test \
		--admin --must-change-password=false >/dev/null 2>&1 || true
	@curl -fsS -o /dev/null -X DELETE -u $(ADMIN):$(PASS) $(URL)/api/v1/users/$(ADMIN)/tokens/forgelab 2>/dev/null || true
	@token=$$(curl -fsS -u $(ADMIN):$(PASS) -H 'Content-Type: application/json' \
		-d '{"name":"forgelab","scopes":["write:organization","write:repository","write:user"]}' \
		$(URL)/api/v1/users/$(ADMIN)/tokens | sed -E 's/.*"sha1":"([^"]+)".*/\1/'); \
	echo "forgejo: $(URL)  ($(ADMIN) / $(PASS))"; \
	echo; \
	echo "  export FORGELAB_LOCAL_TOKEN=$$token"; \
	echo "  cargo run -p forgelab -- apply --sandbox local --fleet examples/fleet"

## Stop the local Forgejo and drop its data
down:
	@docker compose down -v 2>/dev/null || true

## Run the fault proxy in front of the local Forgejo on :3001 (point a sandbox's base_url at it)
proxy:
	cargo run -p forgelab-faultproxy -- --upstream $(URL) --listen 127.0.0.1:3001 --rules faults/mixed.yaml

## Cross-compile the release archives and checksums into ./dist (Linux needs cargo-zigbuild)
dist: dist-linux dist-darwin checksums

dist-linux:
	@mkdir -p dist
	@for p in $(LINUX_TARGETS); do \
		IFS=/ read -r target name <<< "$$p"; \
		echo "  forgelab_$$name.tar.gz"; \
		rustup target add $$target >/dev/null 2>&1; \
		cargo zigbuild --release -p forgelab --target $$target || exit 1; \
		$(MAKE) --no-print-directory archive TARGET=$$target NAME=$$name; \
	done

dist-darwin:
	@mkdir -p dist
	@for p in $(DARWIN_TARGETS); do \
		IFS=/ read -r target name <<< "$$p"; \
		echo "  forgelab_$$name.tar.gz"; \
		rustup target add $$target >/dev/null 2>&1; \
		cargo build --release -p forgelab --target $$target || exit 1; \
		$(MAKE) --no-print-directory archive TARGET=$$target NAME=$$name; \
	done

archive:
	@rm -rf dist/$(NAME) && mkdir -p dist/$(NAME)
	@cp target/$(TARGET)/release/forgelab dist/$(NAME)/forgelab && cp LICENSE dist/$(NAME)/
	@tar -czf dist/forgelab_$(NAME).tar.gz -C dist/$(NAME) forgelab LICENSE
	@rm -rf dist/$(NAME)

checksums:
	@cd dist && (sha256sum *.tar.gz 2>/dev/null || shasum -a 256 *.tar.gz) > checksums.txt
