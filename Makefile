# nix-remote-build-controller developer tasks.
# Most targets assume the Nix dev shell tooling is available; run `nix develop`
# (or `direnv allow`) first, or prefix a target with `nix develop -c`.

GO ?= go
# The Nix toolchain on macOS cannot statically link net/os on darwin; keep cgo off.
export CGO_ENABLED ?= 0

.PHONY: all build test race lint fmt fmt-check vet tidy nixfmt \
        images manifests validate e2e clean help

all: fmt-check vet lint test ## Format-check, vet, lint and test

build: ## Build the Go binaries
	$(GO) build ./...

test: ## Run unit tests
	$(GO) test -count=1 ./...

race: ## Run unit tests with the race detector
	$(GO) test -race -count=1 ./...

vet: ## go vet
	$(GO) vet ./...

lint: ## golangci-lint
	golangci-lint run ./...

fmt: ## Format Go and Nix sources
	gofmt -w .
	nixfmt flake.nix

fmt-check: ## Fail if sources are not formatted
	@out=$$(gofmt -l .); if [ -n "$$out" ]; then echo "gofmt needed:"; echo "$$out"; exit 1; fi

tidy: ## go mod tidy
	$(GO) mod tidy

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
	rm -f result result-* controller proxy
	rm -rf ./bin

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
