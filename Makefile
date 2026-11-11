.PHONY: build frontend test test-backend test-frontend run run-agent clean

BIN := target/release/controldeck
AGENT := target/release/controldeck-agent
# Development: the agent listens in ./run and trusts the invoking user
# (under sudo: the user who ran sudo, so the agent itself can be root).
DEV_SOCKET := $(CURDIR)/run/agent.sock
DEV_USER := $(or $(SUDO_USER),$(shell id -un))

# Compile the SvelteKit frontend (web/), then bundle it into the Rust release binary.
build: frontend
	cargo build --release --workspace

frontend:
	cd web && yarn install --frozen-lockfile && yarn build

# Backend (cargo) and frontend (vitest unit/component + playwright e2e) tests.
test: test-backend test-frontend

test-backend:
	mkdir -p web/build
	cargo test --workspace

test-frontend:
	cd web && yarn install --frozen-lockfile && yarn run check && yarn test

# Web server (unprivileged). Start `make run-agent` in another terminal for package management.
run: $(BIN)
	DECK_AGENT_SOCKET=$(DEV_SOCKET) ./$(BIN)

# Privileged agent for development. Without sudo it runs as you, so package
# changes fail; use `sudo make run-agent` to exercise them for real.
run-agent: $(AGENT)
	DECK_AGENT_SOCKET=$(DEV_SOCKET) DECK_AGENT_USER=$(DEV_USER) ./$(AGENT)

$(BIN) $(AGENT):
	$(MAKE) build

clean:
	cargo clean
	rm -rf web/build web/.svelte-kit
