BIN        := dragonball-evolved
CARGO_BIN  := target/release/$(BIN)
DIST       := dist
DIST_BIN   := $(DIST)/$(BIN)
DIST_DEBUG := $(DIST_BIN).debug

.PHONY: all build release test fmt fmt-check clippy lint clean distclean help

all: build

build:
	cargo build

test:
	cargo test --workspace

fmt:
	cargo +nightly fmt --all

fmt-check:
	cargo +nightly fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

lint: fmt-check clippy

# Produces the shippable pair in $(DIST): a binary that still resolves function
# names on its own, plus a sidecar that adds file:line when it sits next to the
# binary (or in ./.debug/, or /usr/lib/debug). Archive the sidecar per build; it
# is matched to the binary by CRC and fails silently if they drift apart.
release:
	@command -v eu-strip >/dev/null 2>&1 || { \
	    echo "error: eu-strip not found; install elfutils" >&2; exit 1; }
	cargo build --release
	@mkdir -p $(DIST)
	cp $(CARGO_BIN) $(DIST_BIN)
	eu-strip -g -f $(DIST_DEBUG) $(DIST_BIN)

clean:
	cargo clean

distclean: clean
	rm -rf $(DIST)

help:
	@echo 'build      cargo build (debug)'
	@echo 'release    build, split debuginfo into a standalone file'
	@echo 'test       cargo test --workspace'
	@echo 'lint       fmt-check + clippy -D warnings'
	@echo 'clean      cargo clean'
	@echo 'distclean  clean + remove $(DIST)/'
