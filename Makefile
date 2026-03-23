BIN := pr-slack-notifier

.PHONY: build run dry-run lint fmt check clean install docker

build:
	cargo build --release

run:
	cargo run

dry-run:
	cargo run -- --dry-run

lint:
	cargo clippy

fmt:
	cargo fmt

check:
	cargo fmt --check
	cargo clippy -- -D warnings
	cargo build --release

clean:
	cargo clean

install: build
	cp target/release/$(BIN) /usr/local/bin/

docker:
	docker build -t $(BIN) .
