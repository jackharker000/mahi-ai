# Mahi AI — developer entry points.
#
#   make build        Build the whole Rust workspace
#   make test         Run all workspace tests
#   make lint         rustfmt check + clippy (deny warnings) — what CI runs
#   make cli          Run the CLI (in-memory store, mock provider)
#   make daemon       Run the Mac/home-server daemon
#   make xcframework  Build Mahi.xcframework + UniFFI Swift bindings (macOS only)
#   make app          Build the macOS app (xcframework + xcodegen + xcodebuild)
#   make clean        Remove build artifacts

CARGO ?= cargo

.PHONY: build test lint fmt clippy cli daemon xcframework app clean

build:
	$(CARGO) build --workspace

test:
	$(CARGO) test --workspace

lint: fmt clippy

fmt:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

cli:
	./scripts/dev.sh

daemon:
	$(CARGO) run -p mahi-daemon

xcframework:
	./scripts/build-xcframework.sh

app: xcframework
	cd macos && xcodegen generate && xcodebuild build \
		-project Mahi.xcodeproj \
		-scheme Mahi \
		-destination 'platform=macOS'

clean:
	$(CARGO) clean
	rm -rf macos/Frameworks/Mahi.xcframework macos/MahiKit/Generated
