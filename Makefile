# Formatting, in one place.
#
# `make fmt` writes. `make fmt-check` verifies and exits non-zero. CI runs the second, so the ruler
# a developer runs and the gate that rejects their commit are the same two lines, not two checklists
# that drift apart.
#
# Two rulers, both pinned on purpose:
#
#   - rustfmt comes from `rust-toolchain.toml`, so it moves only in a commit that says so.
#   - prettier is pinned exactly in `package.json`/`package-lock.json`, for the same reason. A
#     floating formatter is the same failure as a floating toolchain: one day it reformats files
#     nobody touched, and the commit that goes red is the one that added something else.
#
# What is deliberately not formatted: anything a generator owns. `openapi/v1/openapi.json` is
# produced by `cargo xtask openapi` and checked for zero diff, so a formatter pass over it would
# turn a correct commit into a failing one. See `.prettierignore`.

SHELL := /bin/bash

# The pinned formatter. Built by the rule at the bottom, so `make fmt` on a fresh clone is one
# command rather than "first read the README".
PRETTIER := node_modules/.bin/prettier

# Everything a human wrote. Quoted so prettier expands the globs, not the shell. `node_modules` and
# `target` are excluded by `.prettierignore`, not by the pattern. `--no-error-on-unmatched-pattern`
# keeps a repository without YAML from failing on the rule that would format it if it had any.
PRETTIER_GLOBS := "**/*.md" "**/*.json" "**/*.yml" "**/*.yaml"
PRETTIER_FLAGS := --no-error-on-unmatched-pattern

.DEFAULT_GOAL := fmt

.PHONY: fmt fmt-check fmt-rust fmt-docs setup

# Format everything: sources, then documents.
fmt: fmt-rust fmt-docs

# Verify without writing. This is the CI gate.
fmt-check: $(PRETTIER)
	@if [ -f Cargo.toml ]; then \
		cargo fmt --all --check; \
	else \
		echo "fmt-check: no Cargo.toml yet, skipping cargo fmt"; \
	fi
	@$(PRETTIER) --check $(PRETTIER_FLAGS) $(PRETTIER_GLOBS)

# Rust only. Guarded because the repository is currently documents-first: a missing workspace
# manifest should print one line, not a `cargo` error that reads like a broken checkout.
fmt-rust:
	@if [ -f Cargo.toml ]; then \
		cargo fmt --all; \
	else \
		echo "fmt-rust: no Cargo.toml yet, nothing to format"; \
	fi

# Documents and data only.
fmt-docs: $(PRETTIER)
	@$(PRETTIER) --write $(PRETTIER_FLAGS) $(PRETTIER_GLOBS) --log-level warn

# Install the pinned tools without touching anything else.
setup: $(PRETTIER)

# The binary is the target, so this runs only when the pin moves. `npm ci` rather than
# `npm install`: it installs exactly the lockfile and never edits it, which is what a gate wants.
$(PRETTIER): package.json package-lock.json
	@echo "installing prettier from package-lock.json (npm ci)…"
	@npm ci --silent
