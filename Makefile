# DiscoClip
#
#   make dev     debug build and run
#   make build   release build; the web app in crates/app/ui is built and embedded
#   make run     run the release build
#   make test    run the workspace tests
#   make clean   remove build output
#   make deps    install the web app's packages and fetch the crates

CARGO ?= cargo
NPM   ?= npm
UI    := crates/app/ui
DATA  := data
BIN   := target/release/discoclip

.PHONY: dev build run test clean deps

dev: clean
	$(CARGO) run -- $(ARGS)

build:
	$(CARGO) build --release

run: build
	$(BIN) $(ARGS)

test:
	$(CARGO) test --workspace

clean:
	$(CARGO) clean
	rm -rf $(DATA)

deps:
	cd $(UI) && $(NPM) ci --no-audit --no-fund
	$(CARGO) fetch
