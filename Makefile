TARGET ?= $(shell rustc -vV | sed -n 's/host: //p')
DIST   := dist/onto-$(TARGET)

.PHONY: build test lint fmt release install ffi dist docker demo clean

build:
	cargo build --workspace

test:
	cargo test --workspace

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

demo:
	cargo run -q -p onto-intelli-cli -- --config /tmp/onto-demo/onto-intelli.toml init /tmp/onto-demo --force
	cargo run -q -p onto-intelli-cli -- --config /tmp/onto-demo/onto-intelli.toml seed demo
	cargo run -q -p onto-intelli-cli -- --config /tmp/onto-demo/onto-intelli.toml ask "Why is Project Alpha blocked?" --context

clean:
	cargo clean
	rm -rf dist
