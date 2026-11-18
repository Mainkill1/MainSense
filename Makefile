.PHONY: all test

all:
	cargo build --release --locked --target-dir target
	install -m 755 -T target/release/mainsense ./mainsense

test:
	cargo test --locked
