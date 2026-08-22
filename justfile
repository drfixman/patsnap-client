[private]
default:
    just --list

# Run unit + mock-HTTP + doc tests. Skips ignored live-API tests.
test:
    cargo test

# Live-API smoke tests. Requires the PATSNAP_API_KEY environment variable.
test-live:
    cargo test --test real_patsnap -- --ignored --test-threads=1 --nocapture

# Pre-PR gate: fmt + clippy + tests.
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Requires the nightly toolchain.
fuzz TARGET DURATION="60":
    cargo +nightly fuzz run {{TARGET}} -- -max_total_time={{DURATION}}

# Dry-run publish: verifies the build and the tarball without uploading.
publish-dry:
    cargo publish --dry-run

# Publish to crates.io. Name + version are immutable once accepted.
publish:
    cargo publish
