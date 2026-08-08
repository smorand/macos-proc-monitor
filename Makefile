.PHONY: build release release-dist install uninstall run run-dev clean clean-all \
        info help sync test test-cov lint lint-fix format format-check typecheck \
        security check doc bench bloat deps-dupes \
        daemon-start daemon-stop daemon-status

PROJECT_NAME  = macos-proc-monitor
INSTALL_DIR   = /usr/local/sbin
LAUNCH_LABEL  = com.smorand.macos-proc-monitor
LAUNCH_PLIST  = /Library/LaunchDaemons/$(LAUNCH_LABEL).plist
LOG_DIR       = /var/log/macos-proc-monitor
VERSION       = $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")
ARGS         ?=

## sync: Fetch dependencies and build the workspace
sync:
	cargo fetch
	cargo build --workspace

## build: Build debug binary (all workspace members)
build:
	cargo build --workspace

## release: Build optimized release binary (unwinding; safe for tests/backtraces)
release:
	cargo build --workspace --release

## release-dist: Build the shipped binary (release + panic=abort)
release-dist:
	cargo build -p $(PROJECT_NAME) --profile release-dist

## run: Run the daemon (pass extra flags via ARGS=)
run:
	cargo run -p $(PROJECT_NAME) -- $(ARGS)

## run-dev: Run the daemon with RUST_LOG=debug
run-dev:
	RUST_LOG=debug cargo run -p $(PROJECT_NAME) -- $(ARGS)

## test: Run all tests (nextest if available, else cargo test) + doctests
test:
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --workspace && cargo test --workspace --doc; \
	else \
		echo "cargo-nextest not found, falling back to cargo test (install: cargo install cargo-nextest)"; \
		cargo test --workspace; \
	fi

## test-cov: Run tests with coverage (>= 80% lines)
test-cov:
	@if command -v cargo-llvm-cov >/dev/null 2>&1; then \
		cargo llvm-cov --workspace --fail-under-lines 80; \
	else \
		echo "cargo-llvm-cov not found, running plain tests (install: cargo install cargo-llvm-cov)"; \
		$(MAKE) test; \
	fi

## lint: Clippy with warnings denied (the real lint/type gate)
lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

## lint-fix: Apply clippy + cargo fixes
lint-fix:
	cargo clippy --fix --workspace --allow-dirty --allow-staged
	cargo fix --workspace --allow-dirty --allow-staged

## format: Format all code
format:
	cargo fmt --all

## format-check: Check formatting without writing
format-check:
	cargo fmt --all -- --check

## typecheck: Fast type/borrow check (pre-flight for lint)
typecheck:
	cargo check --workspace --all-targets

## security: Audit advisories + cargo-deny (licenses, bans, sources)
security:
	@if command -v cargo-audit >/dev/null 2>&1; then cargo audit; \
		else echo "cargo-audit not found (install: cargo install cargo-audit)"; fi
	@if command -v cargo-deny >/dev/null 2>&1; then cargo deny check; \
		else echo "cargo-deny not found (install: cargo install cargo-deny)"; fi

## doc: Build docs, fail on broken intra-doc links
doc:
	RUSTDOCFLAGS="-D warnings -D rustdoc::broken_intra_doc_links" cargo doc --workspace --no-deps

## check: Full quality gate (format, lint, typecheck, security, coverage, doc)
check: format-check lint typecheck security test-cov doc

## bench: Run benchmarks (none defined yet)
bench:
	cargo bench --workspace

## bloat: Analyze release binary size by crate
bloat:
	@if command -v cargo-bloat >/dev/null 2>&1; then cargo bloat --release -p $(PROJECT_NAME) --crates; \
		else echo "cargo-bloat not found (install: cargo install cargo-bloat)"; fi

## deps-dupes: List duplicate dependency versions
deps-dupes:
	cargo tree -d

## install: Build release, install binary + sudoers, register & load the launchd daemon (requires sudo)
## Collects metrics to /var/db/macos-proc-monitor/data/ and serves the dashboard on http://127.0.0.1:9090
install: release
	@sudo mkdir -p $(INSTALL_DIR)
	@sudo cp target/release/$(PROJECT_NAME) $(INSTALL_DIR)/$(PROJECT_NAME)
	@sudo chown root:wheel $(INSTALL_DIR)/$(PROJECT_NAME)
	@sudo chmod 755 $(INSTALL_DIR)/$(PROJECT_NAME)
	@echo "Installed $(PROJECT_NAME) to $(INSTALL_DIR)/$(PROJECT_NAME)"
	@sudo cp sudoers.d/$(PROJECT_NAME) /etc/sudoers.d/$(PROJECT_NAME)
	@sudo chmod 440 /etc/sudoers.d/$(PROJECT_NAME)
	@echo "Sudoers rule installed: /etc/sudoers.d/$(PROJECT_NAME)"
	@sudo mkdir -p $(LOG_DIR)
	@sudo mkdir -p /var/db/macos-proc-monitor/data
	@sudo mkdir -p /var/db/macos-proc-monitor/logs
	@sudo cp launchd/$(LAUNCH_LABEL).plist $(LAUNCH_PLIST)
	@sudo chown root:wheel $(LAUNCH_PLIST)
	@sudo chmod 644 $(LAUNCH_PLIST)
	@sudo launchctl unload -w $(LAUNCH_PLIST) 2>/dev/null || true
	@sudo launchctl load -w $(LAUNCH_PLIST)
	@echo "Daemon loaded: $(LAUNCH_LABEL)"
	@echo "Dashboard: http://127.0.0.1:9090"
	@echo "Logs: $(LOG_DIR)/"

## uninstall: Unload the daemon, remove binary + sudoers rule (requires sudo)
uninstall:
	@sudo launchctl unload -w $(LAUNCH_PLIST) 2>/dev/null || true
	@sudo rm -f $(LAUNCH_PLIST)
	@sudo rm -f $(INSTALL_DIR)/$(PROJECT_NAME)
	@sudo rm -f /etc/sudoers.d/$(PROJECT_NAME)
	@echo "Uninstalled $(PROJECT_NAME) (daemon unloaded, binary + sudoers removed)"

# ============================================================================
# DAEMON (launchd)
# ============================================================================

## daemon-start: Start the daemon manually
daemon-start:
	@sudo launchctl start $(LAUNCH_LABEL)
	@echo "Started $(LAUNCH_LABEL)"

## daemon-stop: Stop the daemon (will restart automatically via KeepAlive)
daemon-stop:
	@sudo launchctl stop $(LAUNCH_LABEL)
	@echo "Stopped $(LAUNCH_LABEL) (will auto-restart — use make uninstall to remove)"

## daemon-status: Show daemon status
daemon-status:
	@sudo launchctl list | grep $(LAUNCH_LABEL) || echo "$(LAUNCH_LABEL) not running"

## clean: Remove build artifacts
clean:
	cargo clean

## clean-all: Remove build artifacts and uninstall binary + daemon
clean-all: clean uninstall

## info: Show project info
info:
	@echo "Project:  $(PROJECT_NAME)"
	@echo "Version:  $(VERSION)"
	@echo "Binary:   $(INSTALL_DIR)/$(PROJECT_NAME)"
	@echo "Daemon:   $(LAUNCH_PLIST)"
	@echo "Logs:     $(LOG_DIR)/"

## help: Show this help
help:
	@echo "macos-proc-monitor Makefile"
	@echo "==========================="
	@echo ""
	@grep -E '^## ' Makefile | sed 's/## /  /'
	@echo ""
	@echo "Examples:"
	@echo "  make check                      # full quality gate before commit"
	@echo "  make install                    # build + install binary + register launchd daemon (sudo)"
	@echo "  make uninstall                  # unload daemon + remove binary"
	@echo "  make daemon-status              # check daemon status"
	@echo "  make run ARGS='--help'          # run the daemon with --help"
	@echo "  make run ARGS='--no-slow'       # fast mode, no cwd/fd collection"
	@echo "  make run ARGS='--port 9090'     # run and serve the dashboard on :9090"
