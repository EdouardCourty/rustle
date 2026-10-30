SHELL := /bin/bash
.PHONY: help build test e2e e2e-quick fmt check clippy images up down smoke bench bench-ws bench-all bench-publish bench-scaling data data-check schema-check test-tools install-schema-tool latest latest-3proxy latest-off clean

P ?= rustle
A ?= on
R ?= 3

help: ## list the targets
	@echo "Targets:"
	@grep -E '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## build the binaries (release)
	cargo build --release

test: ## unit tests
	cargo test --release

fmt: ## format all the code
	cargo fmt --all

check: ## fmt + clippy (0 warnings) + tests + site data + quick e2e — run before every commit
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test -p rustle -p bench-client
	$(MAKE) test-tools
	$(MAKE) data-check
	$(MAKE) e2e-quick

e2e: ## black-box functional tests, full suite (~2 min): make e2e [F="filter ..."]
	cargo build --release -p rustle
	cargo run -q --profile e2e -p e2e -- $(F)

e2e-quick: ## functional tests, quick subset (~20 s)
	cargo build --release -p rustle
	cargo run -q --profile e2e -p e2e -- --quick $(F)

clippy: ## linter (warnings = errors)
	cargo clippy --workspace --all-targets -- -D warnings

images: ## build every Docker image (serialized — rustc peaks at ~1-2 GB)
	cd benchmarks && docker compose build generator
	cd benchmarks && docker compose build upstream
	cd benchmarks && for p in rustle 3proxy squid gost tinyproxy; do \
		docker build -q -t $$p:bench -f proxies/$$p/Dockerfile .. >/dev/null && echo "$$p:bench"; done

up: ## start the stack (upstream + sink + proxy)
	cd benchmarks && . ./scripts/env.sh && docker compose up -d upstream sink proxy && sleep 1 && docker ps --format '{{.Names}}\t{{.Status}}' | grep proxybench

down: ## stop the stack
	cd benchmarks && docker compose down

smoke: ## quick test through the stack (http 1kb)
	cd benchmarks && . ./scripts/env.sh && export PROXY_IMAGE=$${P:-rustle}:bench && \
	docker compose --profile generator run --rm -T generator \
		--repeat 1 --no-warmup http http://upstream:8000/1kb --conns 4 --requests 40

bench: ## full matrix: make bench [P=rustle|3proxy|squid|gost|tinyproxy] [A=on|off] [R=3]
	cd benchmarks && REPEAT=$(R) ./scripts/run-bench.sh $(P) $(A)

data: ## regenerate everything published from the raw runs: benchmarks.json, charts, README and RESULTS.md numbers (make data [CAMPAIGN=2026-10-28], default: latest)
	python3 benchmarks/tools/aggregate.py --write $(if $(CAMPAIGN),--campaign $(CAMPAIGN))
	python3 benchmarks/tools/render.py --write $(if $(CAMPAIGN),--campaign $(CAMPAIGN))

test-tools: ## unit tests of the bench tools (record.py, aggregation)
	python3 -m unittest discover -s benchmarks/tools/tests

# The schemas are checked with check-jsonschema, pinned here (CI installs it with `make install-schema-tool`).
CHECK_JSONSCHEMA_VERSION := 0.39.0

data-check: schema-check ## the published data: valid against the JSON Schemas, consistent, and regenerated from the raw runs identical to the committed files
	python3 benchmarks/tools/check.py

schema-check: ## validate the data files against their JSON Schemas (check-jsonschema, or uvx / pipx run)
	@if command -v check-jsonschema >/dev/null; then run="check-jsonschema"; \
	elif command -v uvx >/dev/null; then run="uvx check-jsonschema==$(CHECK_JSONSCHEMA_VERSION)"; \
	elif command -v pipx >/dev/null; then run="pipx run --spec check-jsonschema==$(CHECK_JSONSCHEMA_VERSION) check-jsonschema"; \
	else echo "check-jsonschema not found: pipx install check-jsonschema==$(CHECK_JSONSCHEMA_VERSION)" >&2; exit 1; fi; \
	set -e; \
	$$run --check-metaschema benchmarks/data/schema/*.schema.json documentation/config.schema.json; \
	$$run --schemafile benchmarks/data/schema/index.schema.json benchmarks/data/index.json; \
	$$run --schemafile benchmarks/data/schema/campaign.schema.json benchmarks/data/*/campaign.json; \
	$$run --schemafile benchmarks/data/schema/proxies.schema.json benchmarks/data/*/proxies.json; \
	$$run --schemafile benchmarks/data/schema/benchmarks.schema.json benchmarks/data/*/benchmarks.json; \
	$$run --schemafile benchmarks/data/schema/run.schema.json benchmarks/published/*/runs/*/*/*/run.json; \
	$$run --schemafile documentation/config.schema.json documentation/config.json

install-schema-tool: ## install the pinned check-jsonschema (what CI uses)
	pipx install check-jsonschema==$(CHECK_JSONSCHEMA_VERSION)

latest: ## show the latest result pass of the current proxy
	cd benchmarks && cat results/$(P)/auth-$(A)/$$(ls -t results/$(P)/auth-$(A) | head -1)/run.json

latest-3proxy: latest P=3proxy
latest-off: latest A=off

bench-publish: ## everything: campaigns, raw runs -> benchmarks.json, validity report, charts, README and RESULTS.md numbers (see benchmarks/scripts/run-all.sh)
	cd benchmarks && ./scripts/run-all.sh

bench-ws: ## WebSocket matrix: make bench-ws [P=rustle|squid|gost|…] [A=on|off] [R=3]
	cd benchmarks && MATRIX=ws REPEAT=$(R) ./scripts/run-bench.sh $(P) $(A)

bench-all: ## full campaign: 5 proxies × plain/TLS/h2 (~2-3 h), then tables
	cd benchmarks && AUTH=$(A) ./scripts/run-campaign.sh
	cd benchmarks && python3 tools/aggregate.py --report --sources $$(ls -t results/campaign-[0-9]*.json | head -1)

bench-scaling: ## multi-core curve (needs a many-core proxy host): PROXIES="..." NCPUS="1 2 4 8" make bench-scaling
	cd benchmarks && AUTH=$(A) ./scripts/run-scaling.sh

clean: ## clean Rust artifacts (long rebuild afterwards)
	cargo clean
