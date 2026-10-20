TARGET ?= $(shell rustc -vV | sed -n 's/host: //p')
DIST   := dist/onto-$(TARGET)
# The local database from docker-compose.yml (`make db-up`).
DEV_DATABASE_URL := postgres://onto:onto@127.0.0.1:5441/onto

.PHONY: build test test-db lint fmt release install ffi dist docker demo clean db-up db-down db-reset

build:
	cargo build --workspace

# Tests that need PostgreSQL run when DATABASE_URL is set; `make test-db` sets it.
test:
	cargo test --workspace

test-db: db-up
	DATABASE_URL=$(DEV_DATABASE_URL) cargo test --workspace

# PostgreSQL 16 + pgvector on 127.0.0.1:5441 (docker compose).
db-up:
	docker compose up -d --wait db
	@echo "export ONTO_DATABASE_URL=$(DEV_DATABASE_URL)"

db-down:
	docker compose down

# Stop the database and delete its data.
db-reset:
	docker compose down --volumes
	docker compose up -d --wait db

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

release:
	cargo build --release -p onto-intelli-cli -p onto-intelli-ffi

install:
	cargo install --locked --path crates/onto-cli

ffi:
	cargo build --release -p onto-intelli-ffi
	@ls -lh target/release/libonto_intelli.*

dist:
	cargo build --profile dist --locked -p onto-intelli-cli -p onto-intelli-ffi
	rm -rf $(DIST) && mkdir -p $(DIST)/lib $(DIST)/include
	cp target/dist/onto README.md LICENSE $(DIST)/
	cp crates/onto-ffi/include/onto_intelli.h $(DIST)/include/
	cp target/dist/libonto_intelli.* $(DIST)/lib/ && rm -f $(DIST)/lib/*.d
	tar -czf $(DIST).tar.gz -C dist onto-$(TARGET)
	@shasum -a 256 $(DIST).tar.gz | tee $(DIST).tar.gz.sha256

docker:
	docker build -t onto-intelli .

demo: db-up
	cargo run -q -p onto-intelli-cli -- --config /tmp/onto-demo/onto-intelli.toml init /tmp/onto-demo --force
	cargo run -q -p onto-intelli-cli -- --config /tmp/onto-demo/onto-intelli.toml --database-url $(DEV_DATABASE_URL) seed demo
	cargo run -q -p onto-intelli-cli -- --config /tmp/onto-demo/onto-intelli.toml --database-url $(DEV_DATABASE_URL) ask "Why is Project Alpha blocked?" --context

clean:
	cargo clean
	rm -rf dist
