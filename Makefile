# The browser code is TypeScript; `cargo build` embeds its compiled output, so
# build the web side first (or just use these targets).
web:
	cd web && npx tsc

build: web
	cargo build --release

run: web
	cargo run

test: web
	cargo test

.PHONY: web build run test
