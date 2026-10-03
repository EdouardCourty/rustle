# myproxy — cibles courantes
# `make help` liste tout. Variables utiles :
#   make bench P=3proxy        # matrice sur 3proxy
#   make bench A=off           # matrice sans auth
#   make bench P=myproxy A=off R=5

SHELL := /bin/bash
.PHONY: help build test fmt check clippy images up down smoke bench bench-all bench-scaling latest latest-3proxy latest-off clean

P ?= myproxy
A ?= on
R ?= 3

help: ## afficher les cibles
	@echo "Cibles :"
	@grep -E '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## compiler les deux binaires (release)
	cargo build --release

test: ## tests unitaires
	cargo test --release

fmt: ## formatter tout le code
	cargo fmt --all

check: ## fmt + clippy (0 warning) + tests — à passer avant chaque commit
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test -p myproxy

clippy: ## linter (warnings = erreurs)
	cargo clippy --workspace --all-targets -- -D warnings

images: ## construire toutes les images Docker (sérialisé — rustc pic ~1-2 GB)
	cd bench && docker compose build generator
	cd bench && docker compose build upstream
	cd bench && for p in myproxy 3proxy squid gost tinyproxy; do \
		docker build -q -t $$p:bench -f dut/$$p/Dockerfile .. >/dev/null && echo "$$p:bench"; done

up: ## monter la stack (upstream + sink + proxy)
	cd bench && . ./env.sh && docker compose up -d upstream sink proxy && sleep 1 && docker ps --format '{{.Names}}\t{{.Status}}' | grep proxybench

down: ## éteindre la stack
	cd bench && docker compose down

smoke: ## test rapide via la stack (http 1kb)
	cd bench && . ./env.sh && export PROXY_IMAGE=$${P:-myproxy}:bench && \
	docker compose --profile generator run --rm -T generator \
		--repeat 1 --no-warmup http http://upstream:8000/1kb --conns 4 --requests 40

bench: ## matrice complète : make bench [P=myproxy|3proxy|squid|gost|tinyproxy] [A=on|off] [R=3]
	cd bench && REPEAT=$(R) ./run-bench.sh $(P) $(A)

latest: ## afficher la dernière passe de résultats du proxy courant
	cd bench && cat results/$(P)/auth-$(A)/$$(ls -t results/$(P)/auth-$(A) | head -1)/results.txt

latest-3proxy: latest P=3proxy
latest-off: latest A=off

bench-all: ## campagne complète : 5 proxies × clair/TLS/h2 (~2-3 h) puis tableaux
	cd bench && AUTH=$(A) ./run-campaign.sh
	cd bench && ./tools/campaign-table.py $$(ls -t results/campaign-*.txt | head -1)

bench-scaling: ## courbe multi-core (VPS) : PROXIES="..." NCPUS="1 2 4 8" make bench-scaling
	cd bench && AUTH=$(A) ./run-scaling.sh

clean: ## nettoyer les artefacts rust (rebuild long après)
	cargo clean
