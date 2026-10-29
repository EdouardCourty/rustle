SHELL := /bin/bash
.PHONY: help build test e2e e2e-quick fmt check clippy images up down smoke bench bench-ws bench-all bench-publish bench-scaling charts data data-check latest latest-3proxy latest-off clean

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
	cargo test -p rustle
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
	cd bench && docker compose build generator
	cd bench && docker compose build upstream
	cd bench && for p in rustle 3proxy squid gost tinyproxy; do \
		docker build -q -t $$p:bench -f dut/$$p/Dockerfile .. >/dev/null && echo "$$p:bench"; done

up: ## start the stack (upstream + sink + proxy)
	cd bench && . ./env.sh && docker compose up -d upstream sink proxy && sleep 1 && docker ps --format '{{.Names}}\t{{.Status}}' | grep proxybench

down: ## stop the stack
	cd bench && docker compose down

smoke: ## quick test through the stack (http 1kb)
	cd bench && . ./env.sh && export PROXY_IMAGE=$${P:-rustle}:bench && \
	docker compose --profile generator run --rm -T generator \
		--repeat 1 --no-warmup http http://upstream:8000/1kb --conns 4 --requests 40

bench: ## full matrix: make bench [P=rustle|3proxy|squid|gost|tinyproxy] [A=on|off] [R=3]
	cd bench && REPEAT=$(R) ./run-bench.sh $(P) $(A)

charts: ## regenerate docs/benchmarks/*.svg from a campaign: make charts [C=bench/results/campaign[-ws]-<ts>.txt]
	python3 bench/tools/charts.py $${C:-$$(ls -t bench/results/campaign-[0-9]*.txt | head -1)}

data: ## regenerate bench/data/benchmarks.json from the published campaign (indexes in bench/data/campaign.json)
	python3 bench/tools/site-data.py --write

data-check: ## the website data files (bench/data/, docs/config.json) are up to date and valid
	python3 bench/tools/site-data.py --check

latest: ## show the latest result pass of the current proxy
	cd bench && cat results/$(P)/auth-$(A)/$$(ls -t results/$(P)/auth-$(A) | head -1)/results.txt

latest-3proxy: latest P=3proxy
latest-off: latest A=off

bench-publish: ## everything: campaigns, report, validity check, SVG charts, README table (see bench/run-all.sh)
	cd bench && ./run-all.sh

bench-ws: ## WebSocket matrix: make bench-ws [P=rustle|squid|gost|…] [A=on|off] [R=3]
	cd bench && MATRIX=ws REPEAT=$(R) ./run-bench.sh $(P) $(A)

bench-all: ## full campaign: 5 proxies × plain/TLS/h2 (~2-3 h), then tables
	cd bench && AUTH=$(A) ./run-campaign.sh
	cd bench && ./tools/campaign-table.py $$(ls -t results/campaign-[0-9]*.txt | head -1)

bench-scaling: ## multi-core curve (needs a many-core proxy host): PROXIES="..." NCPUS="1 2 4 8" make bench-scaling
	cd bench && AUTH=$(A) ./run-scaling.sh

clean: ## clean Rust artifacts (long rebuild afterwards)
	cargo clean
