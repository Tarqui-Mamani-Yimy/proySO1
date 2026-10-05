NAME       := epsh
MANIFEST   := crates/epsh/Cargo.toml
TARGET     := $(shell rustc -vV | awk '/^host:/ {print $$2}')

test:
	cargo test --workspace
	perl check.pl -p ./target/debug/epsh -s check-epsh.t

.PHONY: ci
ci:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo build
	cargo test --workspace
	perl check.pl -p ./target/debug/epsh -s check-epsh.t

bench:
	cargo build
	sh crates/epsh/tests/stress/run.sh ./target/debug/epsh dash

lint:
	cargo fmt --all
	cargo clippy --fix --allow-dirty --workspace --all-targets --all-features -- --deny warnings

release:
	cargo clean -p $(NAME) --release --target $(TARGET)
	RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build -p $(NAME) --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

# Usage: make bump-version [V=x.y.z]
# Without V, increments the patch version.
bump-version:
ifndef V
	$(eval OLD := $(shell sed -n 's/^version = "\(.*\)"/\1/p' $(MANIFEST)))
	$(eval V := $(shell echo "$(OLD)" | awk -F. '{printf "%d.%d.%d", $$1, $$2, $$3+1}'))
endif
	@tmp=$$(mktemp) || exit $$?; \
	trap 'rm -f "$$tmp"' 0; \
	trap 'exit 1' 1 2 3 15; \
	sed 's/^version = ".*"/version = "$(V)"/' "$(MANIFEST)" > "$$tmp" && \
	cat "$$tmp" > "$(MANIFEST)"
	cargo check --quiet 2>/dev/null
	git add $(MANIFEST) Cargo.lock
	git commit -m "bump version to $(V)"
	git tag "release/$(V)"
