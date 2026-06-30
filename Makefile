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

eval-validate:
	@cargo run -p anda_brain -- eval \
		--scenario anda_brain/evals/style_preference.json \
		--scenario anda_brain/evals/project_budget.json \
		--scenario anda_brain/evals/preference_reversal.json \
		--scenario anda_brain/evals/fact_correction.json \
		--scenario anda_brain/evals/counterparty_boundary.json \
		--scenario anda_brain/evals/travel_logistics.json \
		--scenario anda_brain/evals/expiring_discount.json \
		--profile anda_brain/evals/no_maintenance_profile.json \
		--profile anda_brain/evals/default_profile.json \
		--profile anda_brain/evals/quick_profile.json \
		--validate-only \
		--summary-only
