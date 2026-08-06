.PHONY: build release install uninstall run clean clean-all info help test

PROJECT_NAME = macos-proc-monitor
INSTALL_DIR  = /usr/local/sbin
VERSION      = $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")

## build: Build debug binary
build:
	cargo build

## release: Build optimized release binary
release:
	cargo build --release

## install: Build release, install to /usr/local/sbin and install sudoers rule (requires sudo)
install: release
	@sudo mkdir -p $(INSTALL_DIR)
	@sudo cp target/release/$(PROJECT_NAME) $(INSTALL_DIR)/$(PROJECT_NAME)
	@sudo chown root:wheel $(INSTALL_DIR)/$(PROJECT_NAME)
	@sudo chmod 755 $(INSTALL_DIR)/$(PROJECT_NAME)
	@echo "Installed to $(INSTALL_DIR)/$(PROJECT_NAME)"
	@sudo cp sudoers.d/$(PROJECT_NAME) /etc/sudoers.d/$(PROJECT_NAME)
	@sudo chmod 440 /etc/sudoers.d/$(PROJECT_NAME)
	@echo "Sudoers rule installed: /etc/sudoers.d/$(PROJECT_NAME)"

## uninstall: Remove binary and sudoers rule (requires sudo)
uninstall:
	@sudo rm -f $(INSTALL_DIR)/$(PROJECT_NAME)
	@sudo rm -f /etc/sudoers.d/$(PROJECT_NAME)
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

## clean-all: Remove build artifacts and uninstall
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
	@echo "  make install                                        # build release + install to /usr/local/sbin (sudo)"
	@echo "  make run ARGS='--help'                             # run with --help"
	@echo "  make run ARGS='--no-slow --interval 2'             # fast mode, no cwd/fd collection"
	@echo "  make run ARGS='--out /tmp/p.jsonl --interval 2'    # custom output file"
