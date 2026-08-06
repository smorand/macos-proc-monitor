.PHONY: build release install uninstall run clean clean-all info help test \
        daemon-install daemon-uninstall daemon-start daemon-stop daemon-status \
        analytics-build analytics-release analytics-install analytics-run install-all

PROJECT_NAME  = macos-proc-monitor
INSTALL_DIR   = /usr/local/sbin
LAUNCH_LABEL  = com.smorand.macos-proc-monitor
LAUNCH_PLIST  = /Library/LaunchDaemons/$(LAUNCH_LABEL).plist
LOG_DIR       = /var/log/macos-proc-monitor
VERSION       = $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")

## build: Build debug binary (all workspace members)
build:
	cargo build

## release: Build optimized release binary (all workspace members)
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

# ============================================================================
# DAEMON (launchd)
# ============================================================================

## daemon-install: Install and load the launchd daemon (requires sudo)
daemon-install: install
	@sudo mkdir -p $(LOG_DIR)
	@sudo cp launchd/$(LAUNCH_LABEL).plist $(LAUNCH_PLIST)
	@sudo chown root:wheel $(LAUNCH_PLIST)
	@sudo chmod 644 $(LAUNCH_PLIST)
	@sudo launchctl load -w $(LAUNCH_PLIST)
	@echo "Daemon loaded: $(LAUNCH_LABEL)"
	@echo "Logs: $(LOG_DIR)/"

## daemon-uninstall: Unload and remove the launchd daemon (requires sudo)
daemon-uninstall:
	@sudo launchctl unload -w $(LAUNCH_PLIST) 2>/dev/null || true
	@sudo rm -f $(LAUNCH_PLIST)
	@echo "Daemon removed: $(LAUNCH_LABEL)"

## daemon-start: Start the daemon manually
daemon-start:
	@sudo launchctl start $(LAUNCH_LABEL)
	@echo "Started $(LAUNCH_LABEL)"

## daemon-stop: Stop the daemon (will restart automatically via KeepAlive)
daemon-stop:
	@sudo launchctl stop $(LAUNCH_LABEL)
	@echo "Stopped $(LAUNCH_LABEL) (will auto-restart — use daemon-uninstall to disable)"

## daemon-status: Show daemon status
daemon-status:
	@sudo launchctl list | grep $(LAUNCH_LABEL) || echo "$(LAUNCH_LABEL) not running"

## run: Run monitor in debug mode (pass extra flags via ARGS=)
run:
	cargo run -p macos-proc-monitor -- $(ARGS)

## test: Run tests
test:
	cargo test

## clean: Remove build artifacts
clean:
	cargo clean

## clean-all: Remove build artifacts and uninstall binary + daemon
clean-all: clean daemon-uninstall uninstall

## info: Show project info
info:
	@echo "Project:  $(PROJECT_NAME)"
	@echo "Version:  $(VERSION)"
	@echo "Binary:   $(INSTALL_DIR)/$(PROJECT_NAME)"
	@echo "Daemon:   $(LAUNCH_PLIST)"
	@echo "Logs:     $(LOG_DIR)/"

# ============================================================================
# ANALYTICS
# ============================================================================

ANALYTICS_INSTALL_DIR = /usr/local/bin

## analytics-build: Build debug analytics binary
analytics-build:
	cargo build -p macos-proc-analytics

## analytics-release: Build release analytics binary
analytics-release:
	cargo build --release -p macos-proc-analytics

## analytics-install: Build release and install analytics to /usr/local/bin (requires sudo)
analytics-install: analytics-release
	@sudo mkdir -p $(ANALYTICS_INSTALL_DIR)
	@sudo cp target/release/macos-proc-analytics $(ANALYTICS_INSTALL_DIR)/macos-proc-analytics
	@sudo chown root:wheel $(ANALYTICS_INSTALL_DIR)/macos-proc-analytics
	@sudo chmod 755 $(ANALYTICS_INSTALL_DIR)/macos-proc-analytics
	@echo "Installed to $(ANALYTICS_INSTALL_DIR)/macos-proc-analytics"

## analytics-run: Run analytics server in dev mode (pass ARGS= for extra flags)
analytics-run:
	cargo run -p macos-proc-analytics -- $(ARGS)

## install-all: Install monitor + analytics + daemon
install-all: install analytics-install daemon-install
	@echo "All components installed"

## help: Show this help
help:
	@echo "macos-proc-monitor Makefile"
	@echo "==========================="
	@echo ""
	@grep -E '^## ' Makefile | sed 's/## /  /'
	@echo ""
	@echo "Examples:"
	@echo "  make install                                        # build release + install to /usr/local/sbin (sudo)"
	@echo "  make daemon-install                                # install + register launchd daemon (boot + auto-restart)"
	@echo "  make daemon-status                                 # check daemon status"
	@echo "  make daemon-stop                                   # stop daemon (auto-restarts)"
	@echo "  make daemon-uninstall                              # stop + remove daemon"
	@echo "  make run ARGS='--help'                             # run with --help"
	@echo "  make run ARGS='--no-slow --interval 2'             # fast mode, no cwd/fd collection"
	@echo "  make run ARGS='--out /tmp/p.jsonl --interval 2'    # custom output file"
