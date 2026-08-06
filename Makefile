.PHONY: build release install uninstall run clean clean-all info help test

PROJECT_NAME = macos-proc-monitor
INSTALL_DIR  = $(HOME)/.local/bin
VERSION      = $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")

## build: Build debug binary
build:
	cargo build

## release: Build optimized release binary
release:
	cargo build --release

## install: Build release and install to ~/.local/bin
install: release
	@mkdir -p $(INSTALL_DIR)
	@cp target/release/$(PROJECT_NAME) $(INSTALL_DIR)/$(PROJECT_NAME)
	@echo "Installed to $(INSTALL_DIR)/$(PROJECT_NAME)"

## uninstall: Remove installed binary
uninstall:
	@rm -f $(INSTALL_DIR)/$(PROJECT_NAME)
	@echo "Uninstalled $(PROJECT_NAME)"

## run: Run in debug mode (pass extra flags via ARGS=)
run:
	cargo run -- $(ARGS)

## test: Run tests
test:
	cargo test

## clean: Remove build artifacts
clean:
	cargo clean

## clean-all: Remove build artifacts and installed binary
clean-all: clean uninstall

## info: Show project info
info:
	@echo "Project: $(PROJECT_NAME)"
	@echo "Version: $(VERSION)"
	@echo "Install: $(INSTALL_DIR)/$(PROJECT_NAME)"

## help: Show this help
help:
	@echo "macos-proc-monitor Makefile"
	@echo "==========================="
	@echo ""
	@grep -E '^## ' Makefile | sed 's/## /  /'
	@echo ""
	@echo "Examples:"
	@echo "  make install                                        # build release + install to ~/.local/bin"
	@echo "  make run ARGS='--help'                             # run with --help"
	@echo "  make run ARGS='--no-slow --interval 2'             # fast mode, no cwd/fd collection"
	@echo "  make run ARGS='--out /tmp/p.jsonl --interval 2'    # custom output file"
