PREFIX ?= /usr/local

.PHONY: build install uninstall test integration deb clean

build:
	cargo build --release

# Delegates to install.sh, which also handles completions and the man page.
install:
	./install.sh

uninstall:
	./install.sh --uninstall

test:
	cargo test

# Real-apt integration tests. Modifies package state — run in a container.
integration: build
	WRAPT=./target/release/wrapt bash tests/integration.sh

# Build a .deb into target/deb/.
deb:
	./scripts/build-deb.sh

clean:
	cargo clean
