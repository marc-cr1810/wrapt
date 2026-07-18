PREFIX ?= /usr/local

.PHONY: build install uninstall test clean

build:
	cargo build --release

# Delegates to install.sh, which also handles shell completions.
install:
	./install.sh

uninstall:
	./install.sh --uninstall

test:
	cargo test

clean:
	cargo clean
