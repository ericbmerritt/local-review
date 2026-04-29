default:
    @just --list

validate: build check-format lint test

build:
    cargo build

[parallel]
check-format: check-for-trailing-whitespace check-format-rust check-format-nix

check-format-rust:
    cargo fmt -- --check

check-format-nix:
    rg --files -g '*.nix' -g '!.*' | xargs alejandra -c

check-for-trailing-whitespace:
    ! rg '\s+$' --glob '!Cargo.lock' --glob '!specs/**'

[parallel]
lint: lint-rust lint-deps lint-nix

lint-rust:
    cargo clippy --all-targets --all-features -- -D warnings

lint-deps:
    cargo deny check

lint-nix:
    rg --files -g '*.nix' -g '!.*' | xargs -L 1 statix check --

test:
    cargo llvm-cov nextest --fail-under-lines 90 \
      --ignore-filename-regex '(^|/)(tui|jj|main|error)\.rs$|tui/(help_screen|composer|composer_overlay)\.rs$'

[parallel]
format: remove-trailing-whitespace format-rust format-nix

format-rust:
    cargo fmt

format-nix:
    rg --files -g '*.nix' -g '!.*' | xargs alejandra

remove-trailing-whitespace:
    files=$(rg -l "\s+$" --glob '!Cargo.lock' --glob '!specs/**' || true); \
    if [ -n "$files" ]; then \
        echo "$files" | xargs sed -i "s/[[:space:]]\+$//"; \
    fi
