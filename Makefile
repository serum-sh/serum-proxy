# Wraps cargo commands inside a Rust toolchain container so we never depend
# on the host having rustc / cargo / pkg-config. Uses `docker run` with named
# volumes for caching.

CARGO_CACHE     := serum-proxy-cargo-cache
TARGET_CACHE    := serum-proxy-target-cache
PROXY_IMAGE     := serum-proxy:dev
TOOLCHAIN_IMAGE := serum-proxy-toolchain:dev

# rustfmt/clippy/pkg-config are baked into the toolchain image (see Dockerfile),
# so the wrapper just runs cargo - no apt or component install per call.
CARGO = docker run --rm \
	-v "$(PWD):/app" \
	-v $(CARGO_CACHE):/usr/local/cargo/registry \
	-v $(TARGET_CACHE):/app/target \
	-w /app \
	$(TOOLCHAIN_IMAGE) \
	cargo

toolchain: ## Build the dev toolchain image if it is missing
	@docker image inspect $(TOOLCHAIN_IMAGE) >/dev/null 2>&1 \
		|| docker build --target toolchain -t $(TOOLCHAIN_IMAGE) .

check: toolchain ## cargo check (binaries + tests)
	@$(CARGO) check --tests --all-targets

build: toolchain ## cargo build (debug)
	@$(CARGO) build --all-targets

release: toolchain ## cargo build (release)
	@$(CARGO) build --release

test: toolchain ## cargo test
	@$(CARGO) test $(filter-out $@,$(MAKECMDGOALS))

fmt: toolchain ## cargo fmt --all
	@$(CARGO) fmt --all

fmt-check: toolchain ## cargo fmt --check
	@$(CARGO) fmt --all -- --check

lint: toolchain ## cargo clippy with warnings as errors
	@$(CARGO) clippy --all-targets -- -D warnings

image: ## Build the runtime Docker image (multi-stage)
	@docker build --target runtime -t $(PROXY_IMAGE) .

run: ## Run the proxy image with the example config (token via env)
	@docker run --rm \
		-e SERUM_PROXY_TOKEN \
		-e ES_TOKEN \
		-v "$(PWD)/config.example.yaml:/app/config.yaml:ro" \
		-p 127.0.0.1:8088:8088 \
		$(PROXY_IMAGE)

clean: ## Clear cargo target cache volume
	@docker volume rm -f $(TARGET_CACHE)

.PHONY: help toolchain check build release test fmt fmt-check lint image run clean
.DEFAULT_GOAL := help

help:
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

# Eat positional arguments so `make test some_test` doesn't error on "some_test".
%:
	@:
