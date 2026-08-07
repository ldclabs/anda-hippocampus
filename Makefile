BUILD_ENV := rust

.PHONY: lint fix test eval-validate

lint:
	@cargo fmt
	@cargo clippy --all-targets --all-features

fix:
	@cargo fmt --all
	@cargo clippy --fix --workspace --tests

test:
	@cargo test --workspace --all-features -- --nocapture

# Every fixture in evals/ is picked up automatically: *_profile.json files
# are profiles, everything else is a scenario. Adding a fixture needs no
# Makefile change; `bundled_eval_fixtures_parse_and_validate` covers the same
# set in `cargo test`.
EVAL_FIXTURES := $(wildcard anda_brain/evals/*.json)
EVAL_PROFILES := $(filter %_profile.json,$(EVAL_FIXTURES))
EVAL_SCENARIOS := $(filter-out %_profile.json,$(EVAL_FIXTURES))

eval-validate:
	@cargo run -p anda_brain --features mcp,wiki -- eval \
		$(foreach scenario,$(EVAL_SCENARIOS),--scenario $(scenario)) \
		$(foreach profile,$(EVAL_PROFILES),--profile $(profile)) \
		--validate-only \
		--summary-only
