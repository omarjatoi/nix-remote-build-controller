# nix-remote-build-controller developer tasks.
# Most targets assume the Nix dev shell tooling is available; run `nix develop`
# (or `direnv allow`) first, or prefix a target with `nix develop -c`.

CARGO ?= cargo

.PHONY: all build test lint fmt fmt-check check nixfmt \
        images manifests validate e2e clean help

all: fmt-check lint test ## Format-check, lint and test

build: ## Build the controller and proxy binaries
	$(CARGO) build --release --locked

test: ## Run unit and integration tests
	$(CARGO) test --locked

lint: ## cargo clippy (warnings are errors)
	$(CARGO) clippy --all-targets --locked -- -D warnings

fmt: ## Format Rust and Nix sources
	$(CARGO) fmt
	nixfmt flake.nix

fmt-check: ## Fail if sources are not formatted
	$(CARGO) fmt --check

check: ## Run the flake checks (format, lint, tests) in the Nix sandbox
	nix flake check

images: ## Build all container images for the host's Linux system
	nix build .#builder-image .#proxy-image .#controller-image

manifests: ## Render the base manifests with kustomize
	kustomize build deploy

validate: ## Validate rendered manifests against the Kubernetes schema
	kustomize build deploy | kubeconform -strict -ignore-missing-schemas -summary
	kustomize build test/e2e/manifests | kubeconform -strict -ignore-missing-schemas -summary

e2e: ## Run the full Kind-based end-to-end test
	bash test/e2e/run.sh

clean: ## Remove build artifacts
	rm -f result result-*
	$(CARGO) clean

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
