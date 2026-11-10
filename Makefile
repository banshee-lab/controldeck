.PHONY: build frontend test test-backend test-frontend run clean

BIN := target/release/controldeck

# Compile the SvelteKit frontend (web/), then bundle it into the Rust release binary.
build: frontend
	cargo build --release

frontend:
	cd web && yarn install --frozen-lockfile && yarn build

# Backend (cargo) and frontend (vitest unit/component + playwright e2e) tests.
test: test-backend test-frontend

test-backend:
	mkdir -p web/build
	cargo test

test-frontend:
	cd web && yarn install --frozen-lockfile && yarn run check && yarn test

run: $(BIN)
	./$(BIN)

$(BIN):
	$(MAKE) build

clean:
	cargo clean
	rm -rf web/build web/.svelte-kit
