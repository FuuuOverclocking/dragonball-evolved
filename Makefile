BIN        := dragonball-evolved
CARGO_BIN  := target/release/$(BIN)
DIST       := dist
DIST_BIN   := $(DIST)/$(BIN)
DIST_DEBUG := $(DIST_BIN).debug
SCHEMA_DIR ?= $(DIST)/schema
CONFIG     ?= examples/vm.toml
PYTHON     ?= python3

.PHONY: all build schema-export schema-check schema-verify release test api-features fmt fmt-check clippy lint clean distclean help

all: build

build:
	cargo build

schema-export:
	cargo run -p api-schema -- export --output-dir "$(SCHEMA_DIR)"

schema-check:
	cargo run -p dragonball-evolved -- check-config "$(CONFIG)"

schema-verify: schema-export
	"$(PYTHON)" scripts/verify-schema.py "$(SCHEMA_DIR)"

test:
	cargo test --workspace

api-features:
	@set -eu; \
	for request in '' request; do \
	for serde in '' serde; do \
	for schema in '' schema; do \
		features="$$request $$serde $$schema"; \
		printf '\nAPI features: [%s]\n' "$$features"; \
		cargo clippy -p api --no-default-features --features "$$features" --all-targets -- -D warnings; \
		cargo test -p api --no-default-features --features "$$features"; \
	done; done; done

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
	@echo 'build          cargo build (debug)'
	@echo 'schema-export  export vm.schema.json and openapi.yaml into SCHEMA_DIR'
	@echo 'schema-check   parse CONFIG and print process settings and commands'
	@echo 'schema-verify  export and validate OpenAPI 3.1 + JSON Schema (PYTHON selects environment)'
	@echo 'release        build, split debuginfo into a standalone file'
	@echo 'test           cargo test --workspace'
	@echo 'api-features   clippy + test all 8 API feature combinations'
	@echo 'lint           fmt-check + clippy -D warnings'
	@echo 'clean          cargo clean'
	@echo 'distclean      clean + remove $(DIST)/'
