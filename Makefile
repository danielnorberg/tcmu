.PHONY: bench bench-large bench-small test clippy

BENCH_BIN = $(shell ls target/release/deps/file_read-* 2>/dev/null | grep -v '\.')

bench: ## Run all benchmarks (requires root)
	cargo bench --features linux-target --bench file_read --no-run
	sudo $(BENCH_BIN) --bench --noplot

bench-large: ## Run only large_file benchmarks
	cargo bench --features linux-target --bench file_read --no-run
	sudo $(BENCH_BIN) --bench --noplot large_file

bench-small: ## Run only small_files benchmarks
	cargo bench --features linux-target --bench file_read --no-run
	sudo $(BENCH_BIN) --bench --noplot small_files

test: ## Run all tests
	cargo test --features linux-target

clippy: ## Run clippy
	cargo clippy --all-targets --all-features -- -D warnings
