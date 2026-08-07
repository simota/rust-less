# rust-less (binary: rless) — build, test, and differential-parity verification.
#
# Parity targets drive tmux, which needs a Unix socket. Agent sandboxes block
# socket creation, so those targets must run outside the sandbox (a plain
# terminal is fine). Targets are grouped below as sandbox-safe vs tmux.

BIN            := target/release/rless
REFERENCE      ?= /usr/bin/less
SCENARIOS      := parity/scenarios
BASELINE       := parity/baseline
HARNESS        := parity/harness
TMP            := parity/tmp
SOURCES        := $(wildcard src/*.rs) Cargo.toml Cargo.lock

.DEFAULT_GOAL := help

# ---------------------------------------------------------------- sandbox-safe

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk -F':.*?## ' '{printf "  \033[1m%-18s\033[0m %s\n", $$1, $$2}'

.PHONY: build
build: ## Debug build
	cargo build

$(BIN): $(SOURCES)
	cargo build --release

.PHONY: release
release: $(BIN) ## Release build (what parity verifies)

.PHONY: test
test: ## Unit + integration tests
	cargo test

.PHONY: fmt
fmt: ## Format sources
	cargo fmt

.PHONY: lint
lint: ## rustfmt check + clippy (warnings are errors)
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

.PHONY: validate
validate: ## Scenario schema + declared-inventory coverage check
	bash $(HARNESS)/validate-scenarios.sh

.PHONY: fixtures
fixtures: ## Regenerate deterministic fixtures (content is stable; SHA256s in parity/provenance.json)
	bash $(HARNESS)/gen-fixtures.sh

.PHONY: scenarios
scenarios: ## Regenerate scenario JSON from the generator table
	bash $(HARNESS)/gen-scenarios.sh

.PHONY: check
check: lint test validate ## Everything that needs no tmux

.PHONY: clean
clean: ## Remove build output and parity scratch dirs (baseline is kept)
	cargo clean
	rm -rf $(TMP)

# ----------------------------------------------------------- tmux (run unsandboxed)

.PHONY: parity
parity: $(BIN) ## Verify rless against the captured less baseline (the ship gate)
	rm -rf $(TMP)/candidate
	bash $(HARNESS)/run.sh $(BIN) $(SCENARIOS) $(TMP)/candidate
	bash $(HARNESS)/diff.sh $(BASELINE) $(TMP)/candidate

.PHONY: drift
drift: ## Re-capture the reference and diff vs the stamped baseline (has the target moved?)
	rm -rf $(TMP)/drift-check
	bash $(HARNESS)/run.sh $(REFERENCE) $(SCENARIOS) $(TMP)/drift-check
	bash $(HARNESS)/diff.sh $(BASELINE) $(TMP)/drift-check

.PHONY: self-check
self-check: ## Prove the harness itself is deterministic (baseline vs baseline recapture)
	rm -rf $(TMP)/self-check
	bash $(HARNESS)/run.sh $(REFERENCE) $(SCENARIOS) $(TMP)/self-check
	bash $(HARNESS)/diff.sh $(BASELINE) $(TMP)/self-check

# Overwrites the fidelity oracle every parity claim rests on. Only legitimate
# when the reference itself changed (confirm with `drift` first) — never to make
# a failing clone pass. Guarded: make baseline CONFIRM=1
.PHONY: baseline
baseline: ## Re-capture the baseline oracle from the reference — requires CONFIRM=1
ifneq ($(CONFIRM),1)
	@echo "refusing: this overwrites $(BASELINE), the oracle all parity claims rest on."
	@echo "run 'make drift' first; if the reference genuinely moved: make baseline CONFIRM=1"
	@exit 1
else
	rm -rf $(BASELINE)
	bash $(HARNESS)/run.sh $(REFERENCE) $(SCENARIOS) $(BASELINE)
	@echo "baseline re-captured — update the provenance stamp in parity/provenance.json"
endif
