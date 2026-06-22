# codemcp Makefile
#
# Common targets:
#   make build        Release build into target/release/codemcp
#   make install      Build and copy the binary onto your PATH
#   make uninstall    Remove the installed binary
#   make dev          Fast debug build
#   make test         Run the test suite
#   make check        Format check + clippy + tests
#   make fmt          Format the code
#   make clean        Remove build artifacts
#
# Cross-compiled release artifacts (used by CI / GitHub releases):
#   make dist                       Build + package every supported target
#   make dist-target TARGET=<rust>  Build + package one target

BIN        := codemcp
CARGO      ?= cargo

# Install location. Override with `make install PREFIX=$HOME/.local`.
PREFIX     ?= /usr/local
BINDIR     ?= $(PREFIX)/bin

# Where packaged release tarballs land.
DIST_DIR   ?= dist

# Supported release targets (Rust triple => asset os/arch suffix).
TARGETS := \
	aarch64-apple-darwin:darwin-arm64 \
	x86_64-apple-darwin:darwin-x86_64 \
	aarch64-unknown-linux-musl:linux-arm64 \
	x86_64-unknown-linux-musl:linux-x86_64

VERSION := $(shell $(CARGO) metadata --no-deps --format-version 1 2>/dev/null \
	| sed -n 's/.*"name":"$(BIN)","version":"\([^"]*\)".*/\1/p')

.PHONY: build dev test check fmt clippy clean install uninstall \
        dist dist-target print-version help

help:
	@grep -E '^#' $(MAKEFILE_LIST) | sed -e 's/^# \{0,1\}//' | sed -n '2,15p'

build:
	$(CARGO) build --release

dev:
	$(CARGO) build

test:
	$(CARGO) test

fmt:
	$(CARGO) fmt

clippy:
	$(CARGO) clippy --all-targets -- -D warnings

check: clippy test
	$(CARGO) fmt --check

clean:
	$(CARGO) clean
	rm -rf $(DIST_DIR)

# Install the locally built release binary onto PATH.
install: build
	@mkdir -p "$(DESTDIR)$(BINDIR)"
	@install -m 0755 target/release/$(BIN) "$(DESTDIR)$(BINDIR)/$(BIN)"
	@echo "installed $(BIN) -> $(DESTDIR)$(BINDIR)/$(BIN)"
	@command -v $(BIN) >/dev/null 2>&1 || \
		echo "warning: $(DESTDIR)$(BINDIR) is not on your PATH; add it so opencode can launch '$(BIN)'"

uninstall:
	@rm -f "$(DESTDIR)$(BINDIR)/$(BIN)"
	@echo "removed $(DESTDIR)$(BINDIR)/$(BIN)"

print-version:
	@echo $(VERSION)

# Build + package every supported target. Requires the relevant Rust targets
# (and, for cross-compiles, a working linker) to be installed.
dist:
	@for entry in $(TARGETS); do \
		$(MAKE) --no-print-directory dist-target \
			TARGET=$${entry%%:*} ASSET=$${entry##*:} || exit 1; \
	done
	@echo "packaged artifacts in $(DIST_DIR)/"

# Build + package one target. Usage: make dist-target TARGET=<triple> [ASSET=<suffix>]
dist-target:
	@test -n "$(TARGET)" || { echo "TARGET is required"; exit 1; }
	@asset="$(ASSET)"; \
	if [ -z "$$asset" ]; then \
		case "$(TARGET)" in \
			aarch64-apple-darwin) asset=darwin-arm64 ;; \
			x86_64-apple-darwin) asset=darwin-x86_64 ;; \
			aarch64-unknown-linux-musl) asset=linux-arm64 ;; \
			x86_64-unknown-linux-musl) asset=linux-x86_64 ;; \
			*) asset="$(TARGET)" ;; \
		esac; \
	fi; \
	echo "==> building $(TARGET) ($$asset)"; \
	rustup target add $(TARGET) >/dev/null 2>&1 || true; \
	$(CARGO) build --release --target $(TARGET); \
	mkdir -p $(DIST_DIR); \
	out="$(DIST_DIR)/$(BIN)-$$asset"; \
	cp "target/$(TARGET)/release/$(BIN)" "$$out"; \
	chmod 0755 "$$out"; \
	tar -C $(DIST_DIR) -czf "$$out.tar.gz" "$(BIN)-$$asset"; \
	rm -f "$$out"; \
	( cd $(DIST_DIR) && shasum -a 256 "$(BIN)-$$asset.tar.gz" > "$(BIN)-$$asset.tar.gz.sha256" ); \
	echo "    -> $(DIST_DIR)/$(BIN)-$$asset.tar.gz"
