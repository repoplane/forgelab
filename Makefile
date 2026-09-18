SHELL := /bin/bash
ADMIN := labadmin
PASS  := labadmin-not-a-secret
URL   := http://localhost:3000

# A release passes VERSION=<tag>; a local build describes the checkout it came from.
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
LDFLAGS := -s -w -X main.version=$(VERSION)

# <GOOS>/<GOARCH>/<name>. The name is what `uname -s`_`uname -m` prints on that platform, so
# the install one-liner in the README can build the asset URL with no script in between.
PLATFORMS := linux/amd64/Linux_x86_64 linux/arm64/Linux_aarch64 \
             darwin/amd64/Darwin_x86_64 darwin/arm64/Darwin_arm64

.PHONY: help build dist lint test unit lock ci up down

## Show this help
help:
	@awk '/^## /{doc=substr($$0,4); next} \
	      /^#/{next} \
	      /^[a-z][a-z-]*:/{if(doc!=""){printf "  \033[1m%-6s\033[0m %s\n", substr($$1,1,length($$1)-1), doc; doc=""}; next} \
	      {doc=""}' $(MAKEFILE_LIST)

## Build ./bin/forgelab
build:
	go build -ldflags "$(LDFLAGS)" -o bin/forgelab ./cmd/forgelab

## Cross-compile release archives and checksums into ./dist
dist:
	@rm -rf dist && mkdir -p dist
	@for p in $(PLATFORMS); do \
		IFS=/ read -r os arch name <<< "$$p"; \
		echo "  forgelab_$$name.tar.gz"; \
		mkdir -p dist/$$name && cp LICENSE dist/$$name/ && \
		CGO_ENABLED=0 GOOS=$$os GOARCH=$$arch go build -trimpath -ldflags "$(LDFLAGS)" \
			-o dist/$$name/forgelab ./cmd/forgelab && \
		tar -czf dist/forgelab_$$name.tar.gz -C dist/$$name forgelab LICENSE && \
		rm -rf dist/$$name || exit 1; \
	done
	@cd dist && (sha256sum *.tar.gz 2>/dev/null || shasum -a 256 *.tar.gz) > checksums.txt

## Check formatting and run go vet
lint:
	@out=$$(gofmt -l .); test -z "$$out" || { echo "not gofmt-clean:"; echo "$$out"; exit 1; }
	go vet ./...

# -count=1 defeats Go's test cache: the suite boots a container and talks to Docker, so a
# cached "ok" would report success for a run that never happened.
## Run every test, including the end-to-end suite (needs Docker)
test:
	go test -count=1 ./...

## Run the tests that need no Docker
unit:
	go test -count=1 -short ./...

## Regenerate examples/fleet/fleet.lock.json after editing the example fleet
lock:
	FORGELAB_UPDATE_LOCK=1 go test -count=1 -run TestWalk ./internal/itest

# The workflow calls the same targets, so a green `make ci` here means a green run there.
## Everything CI runs: lint, then test
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
	echo "  go run ./cmd/forgelab apply --sandbox local --fleet examples/fleet"

## Stop the local Forgejo and drop its data
down:
	@docker compose down -v 2>/dev/null || true
