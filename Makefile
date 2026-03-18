.PHONY: user-flow

user-flow:
	python3 scripts/user_flow_harness.py

.PHONY: slack

slack:
	cargo build --features slack

.PHONY: teams

teams:
	cargo build --features teams
