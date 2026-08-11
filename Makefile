ZIG_VERSION := 0.16.0
ZIG_DIR := .tools/zig-x86_64-linux-$(ZIG_VERSION)
ZIG := $(ZIG_DIR)/zig
ZIG_URL := https://ziglang.org/download/$(ZIG_VERSION)/zig-x86_64-linux-$(ZIG_VERSION).tar.xz
ZIG_SHA256 := 70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00

.PHONY: setup build lint test run

build: setup
	@$(ZIG) build

setup:
	@set -eu; \
	if [ "$$(uname -s)-$$(uname -m)" != Linux-x86_64 ]; then \
		echo "setup supports Linux x86_64 only"; exit 1; \
	fi; \
	if [ -x "$(ZIG)" ] && [ "$$($(ZIG) version 2>/dev/null)" = "$(ZIG_VERSION)" ]; then exit 0; fi; \
	mkdir -p .tools; \
	tmp="$$(mktemp -d .tools/zig-setup.XXXXXX)"; \
	trap 'rm -rf "$$tmp"' EXIT HUP INT TERM; \
	echo "Downloading Zig $(ZIG_VERSION)"; \
	curl --fail --silent --show-error --location "$(ZIG_URL)" -o "$$tmp/zig.tar.xz"; \
	printf '%s  %s\n' "$(ZIG_SHA256)" "$$tmp/zig.tar.xz" | sha256sum --check --status; \
	tar -xJf "$$tmp/zig.tar.xz" -C "$$tmp"; \
	rm -rf "$(ZIG_DIR)"; \
	mv "$$tmp/zig-x86_64-linux-$(ZIG_VERSION)" "$(ZIG_DIR)"

lint: setup
	@$(ZIG) fmt --check build.zig src

test: setup
	@$(ZIG) build test

run: setup
	@$(ZIG) test src/root.zig --test-filter "V0.4 black-box acceptance"
