SHELL := /bin/bash
ADMIN := labadmin
PASS  := labadmin-not-a-secret
URL   := http://localhost:3000

.PHONY: help build test unit up down

## Show this help
help:
	@awk '/^## /{doc=substr($$0,4); next} \
	      /^#/{next} \
	      /^[a-z][a-z-]*:/{if(doc!=""){printf "  \033[1m%-6s\033[0m %s\n", substr($$1,1,length($$1)-1), doc; doc=""}; next} \
	      {doc=""}' $(MAKEFILE_LIST)

## Build ./bin/forgelab
build:
	go build -o bin/forgelab ./cmd/forgelab

# -count=1 defeats Go's test cache: the suite boots a container and talks to Docker, so a
# cached "ok" would report success for a run that never happened.
## Run every test, including the end-to-end suite (needs Docker)
test:
	go test -count=1 ./...

## Run the tests that need no Docker
unit:
	go test -count=1 -short ./...

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
