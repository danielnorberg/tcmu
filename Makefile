.PHONY: bench bench-large bench-small bench-lifecycle test clippy

FIND_BENCH = find target/release/deps -name '$(1)-*' -executable -type f

bench: ## Run all file_read benchmarks (requires root)
	cargo bench --features linux-target --bench file_read --no-run
	sudo $$($(call FIND_BENCH,file_read)) --bench --noplot

bench-large: ## Run only large_file benchmarks
	cargo bench --features linux-target --bench file_read --no-run
	sudo $$($(call FIND_BENCH,file_read)) --bench --noplot large_file

bench-small: ## Run only small_files benchmarks
	cargo bench --features linux-target --bench file_read --no-run
	sudo $$($(call FIND_BENCH,file_read)) --bench --noplot small_files

bench-lifecycle: ## Run device creation/teardown benchmarks (requires root)
	cargo bench --features linux-target --bench device_lifecycle --no-run
	sudo $$($(call FIND_BENCH,device_lifecycle)) --bench --noplot

test: ## Run all tests
	cargo test --features linux-target

clippy: ## Run clippy
	cargo clippy --all-targets --all-features -- -D warnings
