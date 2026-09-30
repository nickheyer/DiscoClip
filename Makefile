# DiscoClip
#
#   make dev        debug build and run
#   make build      release build; the web app in crates/app/ui is built and embedded
#   make run        run the release build
#   make test       run the workspace tests. The resolver tests that reach the live
#                   sites are marked #[ignore]; `make test-live` runs them
#   make check      formatting, clippy, the tests and the web app's checks, as CI runs them
#   make fmt        format the Rust code and the web app
#   make clean      remove build output
#   make deps       install the web app's packages and fetch the crates
#   make image      build the Docker image, discoclip:latest
#   make image-gpu  build the Docker image with the hardware-capable ffmpeg, discoclip:gpu
#   make smoke      build both images and run packaging/smoke-test.sh on each
#   make dist       the release archive for this machine, in dist/
#   make deb        the .deb for this machine, in dist/. Needs nfpm
#   make rpm        the .rpm for this machine, in dist/. Needs nfpm

CARGO ?= cargo
NPM   ?= npm
UI    := crates/app/ui
DATA  := data
DIST  := dist
BIN   := target/release/discoclip

UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
OS      := $(if $(filter Darwin,$(UNAME_S)),macos,linux)
ARCH    := $(if $(filter arm64 aarch64,$(UNAME_M)),aarch64,x86_64)
PKGARCH := $(if $(filter aarch64,$(ARCH)),arm64,amd64)

.PHONY: dev build run test test-live check fmt clean deps image image-gpu smoke dist deb rpm

dev: clean
	$(CARGO) run -- $(ARGS)

build:
	$(CARGO) build --release --locked

run: build
	$(BIN) $(ARGS)

test:
	$(CARGO) test --workspace --locked

test-live:
	$(CARGO) test --workspace --locked -- --ignored

check:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings
	$(CARGO) test --workspace --locked
	cd $(UI) && $(NPM) run check && $(NPM) run lint

fmt:
	$(CARGO) fmt --all
	cd $(UI) && $(NPM) run format

clean:
	$(CARGO) clean
	rm -rf $(DATA) $(DIST)

deps:
	cd $(UI) && $(NPM) ci --no-audit --no-fund
	$(CARGO) fetch

image:
	docker build --target cpu -t discoclip:latest .

image-gpu:
	docker build --target gpu -t discoclip:gpu .

smoke: image image-gpu
	packaging/smoke-test.sh discoclip:latest cpu
	packaging/smoke-test.sh discoclip:gpu gpu

dist: build
	packaging/archive.sh $(BIN) $(OS)-$(ARCH) $(DIST)

deb: build
	packaging/package.sh $(BIN) $(PKGARCH) $(DIST) deb

rpm: build
	packaging/package.sh $(BIN) $(PKGARCH) $(DIST) rpm
