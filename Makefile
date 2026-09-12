# DiscoClip
#
#   make dev     debug build and run
#   make build   release build; the web app in crates/app/ui is built and embedded
#   make run     run the release build
#   make clean   remove build output
#   make deps    install the web app's packages and fetch the crates

CARGO ?= cargo
NPM   ?= npm
UI    := crates/app/ui
BIN   := target/release/discoclip

.PHONY: dev build run clean deps

dev:
	$(CARGO) run -- $(ARGS)

build:
	$(CARGO) build --release

run: build
	$(BIN) $(ARGS)

clean:
	$(CARGO) clean
	rm -rf $(UI)/.svelte-kit

deps:
	cd $(UI) && $(NPM) ci --no-audit --no-fund
	$(CARGO) fetch
