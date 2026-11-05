CARGO ?= cargo
PERF ?= perf
PERF_BYTES ?= 64
PERF_FD ?= false
PERF_ITERATIONS ?= 1000000

.PHONY: build fmt lint feature-check example bench perf

build:
	$(CARGO) build --all-targets

fmt:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --all-targets --all-features -- -D warnings

feature-check:
	$(CARGO) check --lib --no-default-features
	$(CARGO) check --lib --no-default-features --features json
	$(CARGO) clippy --all-targets --no-default-features -- -D warnings
	$(CARGO) clippy --all-targets --no-default-features --features json -- -D warnings

example:
	$(CARGO) run --example rpc

bench:
	$(CARGO) run --release --example latency

perf:
	CARGO_TARGET_DIR=target/perf CARGO_PROFILE_RELEASE_DEBUG=2 RUSTFLAGS="$(RUSTFLAGS) -C force-frame-pointers=yes" $(CARGO) build --release --example latency
	PROFILE_BYTES=$(PERF_BYTES) PROFILE_FD=$(PERF_FD) ITERATIONS=$(PERF_ITERATIONS) $(PERF) record -e cycles:u -F 997 --call-graph fp -o target/perf/rpc.data -- target/perf/release/examples/latency
	$(PERF) report --stdio --no-children --call-graph none --percent-limit 1 -i target/perf/rpc.data
